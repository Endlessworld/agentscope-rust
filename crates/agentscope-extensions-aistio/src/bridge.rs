//! The reporting bridge, mirroring `io.agentscope.extensions.aistio.SessionBridge`.
//!
//! This is the P0/P1 skeleton: it holds the config, mounts a [`FrameworkAdapter`],
//! assigns monotonic per-session sequence numbers to emitted Level-2 events, and
//! journals them durably. Level-1/4 upstream reporting over ASDP gRPC is deferred (see
//! tracker §4.2); the in-process contract surface is exposed via [`ContractProvider`] so
//! the HTTP server can be layered on later.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;

use crate::adapter::FrameworkAdapter;
use crate::config::AistioConfig;
use crate::error::AistioResult;
use crate::event_journal::EventJournal;
use crate::model::{ContextSnapshot, ContextTracker, Inventory, MessagePage, SessionEvent, ToolInfo};
use crate::transport::ContractProvider;

/// Session metadata seeded by the adapter on the session's first turn.
///
/// None of it is carried by the Level-2 event stream, so the adapter reports it once
/// (mirrors `SessionBridge.describeSession`).
#[derive(Debug, Clone, Default)]
pub struct SessionDescription {
    /// Base system prompt bound to the agent.
    pub system_prompt: String,
    /// Tools exposed to the model.
    pub tools: Vec<ToolInfo>,
    /// Context-window maximum, tokens.
    pub context_window: i64,
    /// Model identifier.
    pub model_name: String,
}

/// Mutable bridge state, shared behind `Arc<Mutex<..>>` so the adapter's emit closure
/// (which the middleware holds as a `Fn`) folds events in without an `&mut` bridge.
#[derive(Default)]
struct BridgeState {
    journal: Option<EventJournal>,
    trackers: HashMap<String, ContextTracker>,
    descriptions: HashMap<String, SessionDescription>,
    busy: HashSet<String>,
    effective_prompts: HashMap<String, String>,
    context_used: HashMap<String, i64>,
}

impl BridgeState {
    /// Assigns the next per-session sequence, journals the event and folds it into the
    /// tracker. Shared by [`AistioBridge`] and [`BridgeReporter`].
    fn publish(&mut self, agent_key: &str, mut event: SessionEvent) -> i64 {
        let next = self.next_seq(&event.session_id);
        event.set_seq(next);
        if let Some(journal) = self.journal.as_mut() {
            let _ = journal.append(event.clone());
        }
        let session_id = event.session_id.clone();
        self.trackers
            .entry(session_id.clone())
            .or_insert_with(|| ContextTracker::new(&session_id, agent_key))
            .apply(&event);
        next
    }

    /// Next monotonic sequence for a session, derived from the durable journal when
    /// event reporting is enabled, otherwise from the in-memory tracker.
    fn next_seq(&self, session_id: &str) -> i64 {
        match self.journal.as_ref() {
            Some(journal) => journal.latest_sequence(session_id) + 1,
            None => self
                .trackers
                .get(session_id)
                .map(|t| t.message_count() + 1)
                .unwrap_or(1),
        }
    }

    /// Seeds a session's metadata into its tracker.
    fn describe(
        &mut self,
        agent_key: &str,
        session_id: &str,
        system_prompt: String,
        tools: Vec<ToolInfo>,
        context_window: i64,
        model_name: String,
    ) {
        let desc = SessionDescription {
            system_prompt,
            tools,
            context_window,
            model_name,
        };
        self.descriptions.insert(session_id.to_string(), desc.clone());
        let tracker = self
            .trackers
            .entry(session_id.to_string())
            .or_insert_with(|| ContextTracker::new(session_id, agent_key));
        tracker.set_system_prompt(desc.system_prompt);
        tracker.set_tools(desc.tools);
        tracker.set_max_tokens(desc.context_window);
        tracker.set_model(desc.model_name);
    }

    /// Sets tokens currently occupying the effective context window.
    fn set_context_used(&mut self, agent_key: &str, session_id: &str, tokens: i64) {
        self.context_used.insert(session_id.to_string(), tokens.max(0));
        self.trackers
            .entry(session_id.to_string())
            .or_insert_with(|| ContextTracker::new(session_id, agent_key))
            .set_context_used_tokens(tokens);
    }
}

/// A cloneable handle sharing a bridge's event/journal state, handed to adapters so they
/// can publish events and seed session metadata without owning the bridge.
#[derive(Clone)]
pub struct BridgeReporter {
    agent_key: String,
    state: Arc<Mutex<BridgeState>>,
}

impl BridgeReporter {
    fn lock_state(&self) -> MutexGuard<'_, BridgeState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Publishes one event, assigning its sequence and journalling it (mirrors
    /// `SessionBridge.publish`). Never panics: a failed report must not disturb the agent.
    pub fn publish(&self, event: SessionEvent) -> i64 {
        self.lock_state().publish(&self.agent_key, event)
    }

    /// Seeds session metadata (mirrors `SessionBridge.describeSession`).
    pub fn describe_session(
        &self,
        session_id: &str,
        system_prompt: impl Into<String>,
        tools: Vec<ToolInfo>,
        context_window: i64,
        model_name: impl Into<String>,
    ) {
        self.lock_state().describe(
            &self.agent_key,
            session_id,
            system_prompt.into(),
            tools,
            context_window,
            model_name.into(),
        );
    }

    /// Marks whether a turn is in progress (mirrors `SessionBridge.setBusy`).
    pub fn set_busy(&self, session_id: &str, busy: bool) {
        let mut state = self.lock_state();
        if busy {
            state.busy.insert(session_id.to_string());
        } else {
            state.busy.remove(session_id);
        }
    }

    /// Stores the middleware-composed system prompt (mirrors
    /// `SessionBridge.setEffectiveSystemPrompt`).
    pub fn set_effective_system_prompt(&self, session_id: &str, prompt: impl Into<String>) {
        self.lock_state()
            .effective_prompts
            .insert(session_id.to_string(), prompt.into());
    }

    /// The middleware-sampled effective system prompt for `session_id`, or empty.
    pub fn effective_system_prompt(&self, session_id: &str) -> String {
        self.lock_state()
            .effective_prompts
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Sets tokens occupying the effective context window (mirrors
    /// `SessionBridge.setContextUsedTokens`).
    pub fn set_context_used_tokens(&self, session_id: &str, tokens: i64) {
        self.lock_state().set_context_used(&self.agent_key, session_id, tokens);
    }
}

/// Reporting bridge for one instrumented agent.
///
/// Mirrors the Java `SessionBridge`: `new` -> [`attach`](Self::attach) ->
/// [`start`](Self::start), and [`close`](Self::close) to detach and release channels.
pub struct AistioBridge {
    config: AistioConfig,
    adapter: Option<Box<dyn FrameworkAdapter>>,
    state: Arc<Mutex<BridgeState>>,
    started: AtomicBool,
}

impl AistioBridge {
    /// Creates a bridge without attaching or starting it, mirroring `Aistio.newBridge`.
    pub fn new(config: AistioConfig) -> Self {
        Self {
            config,
            adapter: None,
            state: Arc::new(Mutex::new(BridgeState::default())),
            started: AtomicBool::new(false),
        }
    }

    /// Locks the shared bridge state, recovering from poisoning (a failed report must
    /// never take the bridge down).
    fn lock_state(&self) -> MutexGuard<'_, BridgeState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The bridge config.
    pub fn config(&self) -> &AistioConfig {
        &self.config
    }

    /// Whether the bridge has been started.
    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    /// Returns a snapshot of the tracker for `session_id`, creating an empty one on
    /// first sight (mirrors `SessionBridge.tracker`).
    pub fn tracker(&self, session_id: &str) -> ContextTracker {
        let framework = self.config.agent_key.clone();
        self.lock_state()
            .trackers
            .entry(session_id.to_string())
            .or_insert_with(|| ContextTracker::new(session_id, framework))
            .clone()
    }

    /// Attaches an adapter and wires its event emitter to this bridge.
    ///
    /// The emitted closure only forwards events (it never assigns sequence numbers); the
    /// caller records them through [`record_event`](Self::record_event).
    pub fn attach(
        &mut self,
        target: Arc<dyn std::any::Any + Send + Sync>,
        mut adapter: Box<dyn FrameworkAdapter>,
    ) -> AistioResult<()> {
        if !adapter.can_handle(target.as_ref()) {
            return Err(crate::error::AistioError::Config(
                "adapter cannot handle the provided target".into(),
            ));
        }
        if self.config.enable_events && self.lock_state().journal.is_none() {
            let journal = EventJournal::new(&self.config)?;
            self.lock_state().journal = Some(journal);
        }
        // The adapter publishes through this closure; folding the event in is the whole
        // point of the bridge. A failed report must never break the agent's path, so the
        // closure never panics and swallows journal errors.
        let reporter = self.reporter();
        let emit_reporter = reporter.clone();
        let emit: Arc<dyn Fn(SessionEvent) + Send + Sync> =
            Arc::new(move |event: SessionEvent| {
                emit_reporter.publish(event);
            });
        adapter.attach(target, emit)?;
        adapter.on_bridge_attached(reporter);
        self.adapter = Some(adapter);
        Ok(())
    }

    /// Starts the bridge (begins accepting Level-2 events into the journal).
    pub fn start(self) -> AistioResult<Self> {
        if self.config.enable_events && self.lock_state().journal.is_none() {
            let journal = EventJournal::new(&self.config)?;
            self.lock_state().journal = Some(journal);
        }
        self.started.store(true, Ordering::SeqCst);
        Ok(self)
    }

    /// Records one adapter-originated event, assigning the next per-session sequence and
    /// journalling it when event reporting is enabled.
    pub fn record_event(&self, event: SessionEvent) -> i64 {
        self.lock_state().publish(&self.config.agent_key, event)
    }

    /// A cloneable handle sharing this bridge's event/journal state, handed to adapters so
    /// they can publish events and seed session metadata without owning the bridge.
    pub fn reporter(&self) -> BridgeReporter {
        BridgeReporter {
            agent_key: self.config.agent_key.clone(),
            state: Arc::clone(&self.state),
        }
    }

    /// Seeds a session's metadata (system prompt, tools, context window, model), which
    /// the Level-2 event stream does not carry. Mirrors `SessionBridge.describeSession`.
    pub fn describe_session(
        &self,
        session_id: &str,
        system_prompt: impl Into<String>,
        tools: Vec<ToolInfo>,
        context_window: i64,
        model_name: impl Into<String>,
    ) {
        let agent_key = self.config.agent_key.clone();
        self.lock_state().describe(
            &agent_key,
            session_id,
            system_prompt.into(),
            tools,
            context_window,
            model_name.into(),
        );
    }

    /// Marks whether a turn is in progress for `session_id`.
    pub fn set_busy(&self, session_id: &str, busy: bool) {
        if busy {
            self.lock_state().busy.insert(session_id.to_string());
        } else {
            self.lock_state().busy.remove(session_id);
        }
    }

    /// Whether a turn is currently in progress for `session_id`.
    pub fn is_busy(&self, session_id: &str) -> bool {
        self.lock_state().busy.contains(session_id)
    }

    /// Stores the middleware-composed system prompt for `session_id`.
    pub fn set_effective_system_prompt(&self, session_id: &str, prompt: impl Into<String>) {
        self.lock_state()
            .effective_prompts
            .insert(session_id.to_string(), prompt.into());
    }

    /// The middleware-sampled effective system prompt for `session_id`, or empty.
    pub fn effective_system_prompt(&self, session_id: &str) -> String {
        self.lock_state()
            .effective_prompts
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Sets tokens currently occupying the effective context window for `session_id`.
    pub fn set_context_used_tokens(&self, session_id: &str, tokens: i64) {
        let agent_key = self.config.agent_key.clone();
        self.lock_state().set_context_used(&agent_key, session_id, tokens);
    }

    /// Whether `session_id` has been seen by this bridge.
    pub fn is_known_session(&self, session_id: &str) -> bool {
        self.lock_state().trackers.contains_key(session_id)
    }

    /// Detaches the adapter and releases channels, mirroring `SessionBridge.close`.
    pub fn close(&mut self) -> AistioResult<()> {
        if let Some(mut adapter) = self.adapter.take() {
            let _ = adapter.detach();
        }
        if let Some(journal) = self.lock_state().journal.as_mut() {
            journal.ack_all();
        }
        self.started.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Materializes the Level-4 context snapshot for a session.
    pub fn context_snapshot(&self, session_id: &str) -> AistioResult<ContextSnapshot> {
        let framework = self.config.agent_key.clone();
        let state = self.lock_state();
        match state.trackers.get(session_id) {
            Some(tracker) => Ok(tracker.snapshot()),
            None => Ok(ContextSnapshot::builder(session_id)
                .framework(framework)
                .build()),
        }
    }
}

#[async_trait]
impl ContractProvider for AistioBridge {
    async fn inventory(&self) -> AistioResult<Inventory> {
        Ok(Inventory::health_only(self.lock_state().trackers.len() as i64))
    }

    async fn context(&self, session_id: &str) -> AistioResult<ContextSnapshot> {
        self.context_snapshot(session_id)
    }

    async fn messages(
        &self,
        session_id: &str,
        offset: i64,
        limit: i64,
    ) -> AistioResult<MessagePage> {
        // Level-3 full history is served by the adapter when it declares `message-query`.
        let items = match &self.adapter {
            Some(adapter) => adapter
                .list_messages(session_id, offset, limit)
                .await
                .map(|p| p.messages)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        Ok(MessagePage::of(session_id, items, offset, limit))
    }

    async fn session_ids(&self) -> AistioResult<Vec<String>> {
        Ok(self.lock_state().trackers.keys().cloned().collect())
    }

    async fn info(&self) -> AistioResult<serde_json::Value> {
        let adapter = self.adapter.as_ref();
        let mut cfg = serde_json::Map::new();
        cfg.insert("name".into(), self.config.agent_key.clone().into());
        cfg.insert("sources".into(), serde_json::json!(["bridge"]));
        Ok(serde_json::json!({
            "agentKey": self.config.agent_key,
            "tenant": self.config.tenant,
            "namespace": self.config.namespace,
            "instanceKey": self.config.instance_key,
            "framework": adapter.map(|a| a.framework_name()).unwrap_or(""),
            "frameworkVersion": adapter.map(|a| a.framework_version()).unwrap_or(""),
            "capabilities": adapter.map(|a| a.capabilities()).unwrap_or_default(),
            "agentConfig": cfg,
        }))
    }

    async fn subagents(&self) -> AistioResult<Vec<serde_json::Value>> {
        let inv = self.inventory().await?;
        Ok(inv.subagents.iter().map(|s| s.to_json_value()).collect())
    }

    async fn workspaces(&self) -> AistioResult<Vec<serde_json::Value>> {
        let inv = self.inventory().await?;
        Ok(inv
            .workspaces
            .iter()
            .map(|w| serde_json::json!({ "path": w.path, "mode": w.mode }))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::session_event::TYPE_MESSAGE;

    fn temp_bridge() -> AistioBridge {
        let cfg = AistioConfig::builder("friday")
            .instance_key("rep-1")
            .event_journal_dir(
                std::env::temp_dir()
                    .join(format!("aistio-bridge-{}", uuid::Uuid::new_v4()))
                    .to_str()
                    .unwrap(),
            )
            .build()
            .unwrap();
        AistioBridge::new(cfg).start().unwrap()
    }

    #[test]
    fn record_event_assigns_monotonic_seq() {
        let bridge = temp_bridge();
        let s1 = bridge.record_event(SessionEvent::builder("s1", TYPE_MESSAGE).content("a").build());
        let s2 = bridge.record_event(SessionEvent::builder("s1", TYPE_MESSAGE).content("b").build());
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
        assert_eq!(bridge.tracker("s1").effective_message_count(), 2);
    }

    #[test]
    fn describe_session_seeds_tracker_metadata() {
        let bridge = temp_bridge();
        bridge.describe_session(
            "s1",
            "you are helpful",
            vec![ToolInfo::new("search", "web search")],
            128_000,
            "gpt-4o",
        );
        let snap = bridge.context_snapshot("s1").unwrap();
        assert_eq!(snap.system_prompt, "you are helpful");
        assert_eq!(snap.tools.len(), 1);
        assert_eq!(snap.max_tokens, 128_000);
        assert_eq!(snap.model, "gpt-4o");
    }

    #[test]
    fn busy_and_effective_prompt_round_trip() {
        let bridge = temp_bridge();
        assert!(!bridge.is_busy("s1"));
        bridge.set_busy("s1", true);
        assert!(bridge.is_busy("s1"));
        bridge.set_busy("s1", false);
        assert!(!bridge.is_busy("s1"));
        bridge.set_effective_system_prompt("s1", "composed");
        assert_eq!(bridge.effective_system_prompt("s1"), "composed");
        assert_eq!(bridge.effective_system_prompt("missing"), "");
    }

    #[test]
    fn set_context_used_tokens_updates_snapshot() {
        let bridge = temp_bridge();
        bridge.set_context_used_tokens("s1", 4096);
        let snap = bridge.context_snapshot("s1").unwrap();
        assert_eq!(snap.total_tokens, 4096);
        assert!(bridge.is_known_session("s1"));
        assert!(!bridge.is_known_session("nope"));
    }

    #[tokio::test]
    async fn contract_provider_reports_empty_inventory() {
        let cfg = AistioConfig::builder("friday").instance_key("rep-1").build().unwrap();
        let bridge = AistioBridge::new(cfg);
        let inv = bridge.inventory().await.unwrap();
        assert!(inv.subagents.is_empty());
    }
}
