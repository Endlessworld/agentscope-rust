//! The reporting bridge, mirroring `io.agentscope.extensions.aistio.SessionBridge`.
//!
//! This is the P0/P1 skeleton: it holds the config, mounts a [`FrameworkAdapter`],
//! assigns monotonic per-session sequence numbers to emitted Level-2 events, and
//! journals them durably. Level-1/4 upstream reporting over ASDP gRPC is deferred (see
//! tracker §4.2); the in-process contract surface is exposed via [`ContractProvider`] so
//! the HTTP server can be layered on later.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use crate::adapter::FrameworkAdapter;
use crate::config::AistioConfig;
use crate::error::AistioResult;
use crate::event_journal::EventJournal;
use crate::model::{ContextSnapshot, ContextTracker, Inventory, MessagePage, SessionEvent};
use crate::transport::ContractProvider;

/// Reporting bridge for one instrumented agent.
///
/// Mirrors the Java `SessionBridge`: `new` -> [`attach`](Self::attach) ->
/// [`start`](Self::start), and [`close`](Self::close) to detach and release channels.
pub struct AistioBridge {
    config: AistioConfig,
    adapter: Option<Box<dyn FrameworkAdapter>>,
    journal: Option<EventJournal>,
    trackers: HashMap<String, ContextTracker>,
    started: AtomicBool,
}

impl AistioBridge {
    /// Creates a bridge without attaching or starting it, mirroring `Aistio.newBridge`.
    pub fn new(config: AistioConfig) -> Self {
        Self {
            config,
            adapter: None,
            journal: None,
            trackers: HashMap::new(),
            started: AtomicBool::new(false),
        }
    }

    /// The bridge config.
    pub fn config(&self) -> &AistioConfig {
        &self.config
    }

    /// Whether the bridge has been started.
    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    /// Interns a tracker for `session_id`, creating one on first sight.
    pub fn tracker(&mut self, session_id: &str) -> &mut ContextTracker {
        let framework = self.config.agent_key.clone();
        self.trackers
            .entry(session_id.to_string())
            .or_insert_with(|| ContextTracker::new(session_id, framework))
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
        if self.config.enable_events && self.journal.is_none() {
            self.journal = Some(EventJournal::new(&self.config)?);
        }
        // A failed report must never break the agent's path; the emitter swallows.
        let emit: Arc<dyn Fn(SessionEvent) + Send + Sync> = Arc::new(|_event: SessionEvent| {});
        adapter.attach(target, emit)?;
        self.adapter = Some(adapter);
        Ok(())
    }

    /// Starts the bridge (begins accepting Level-2 events into the journal).
    pub fn start(mut self) -> AistioResult<Self> {
        if self.config.enable_events && self.journal.is_none() {
            self.journal = Some(EventJournal::new(&self.config)?);
        }
        self.started.store(true, Ordering::SeqCst);
        Ok(self)
    }

    /// Next monotonic sequence for a session, derived from the durable journal when
    /// event reporting is enabled, otherwise from the in-memory tracker.
    fn next_seq(&self, session_id: &str) -> i64 {
        if let Some(journal) = self.journal.as_ref() {
            journal.latest_sequence(session_id) + 1
        } else {
            self.trackers
                .get(session_id)
                .map(|t| t.message_count() + 1)
                .unwrap_or(1)
        }
    }

    /// Records one adapter-originated event, assigning the next per-session sequence and
    /// journalling it when event reporting is enabled.
    pub fn record_event(&mut self, mut event: SessionEvent) -> i64 {
        let next = self.next_seq(&event.session_id);
        event.set_seq(next);
        if let Some(journal) = self.journal.as_mut() {
            let _ = journal.append(event.clone());
        }
        let session_id = event.session_id.clone();
        self.tracker(&session_id).apply(&event);
        next
    }

    /// Detaches the adapter and releases channels, mirroring `SessionBridge.close`.
    pub fn close(&mut self) -> AistioResult<()> {
        if let Some(mut adapter) = self.adapter.take() {
            let _ = adapter.detach();
        }
        if let Some(journal) = self.journal.as_mut() {
            journal.ack_all();
        }
        self.started.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Materializes the Level-4 context snapshot for a session.
    pub fn context_snapshot(&self, session_id: &str) -> AistioResult<ContextSnapshot> {
        match self.trackers.get(session_id) {
            Some(tracker) => Ok(tracker.snapshot()),
            None => Ok(ContextSnapshot::builder(session_id)
                .framework(self.config.agent_key.clone())
                .build()),
        }
    }
}

#[async_trait]
impl ContractProvider for AistioBridge {
    async fn inventory(&self) -> AistioResult<Inventory> {
        Ok(Inventory::health_only(self.trackers.len() as i64))
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
        Ok(self.trackers.keys().cloned().collect())
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
        let mut bridge = temp_bridge();
        let s1 = bridge.record_event(SessionEvent::builder("s1", TYPE_MESSAGE).content("a").build());
        let s2 = bridge.record_event(SessionEvent::builder("s1", TYPE_MESSAGE).content("b").build());
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
        assert_eq!(bridge.tracker("s1").effective_message_count(), 2);
    }

    #[tokio::test]
    async fn contract_provider_reports_empty_inventory() {
        let cfg = AistioConfig::builder("friday").instance_key("rep-1").build().unwrap();
        let bridge = AistioBridge::new(cfg);
        let inv = bridge.inventory().await.unwrap();
        assert!(inv.subagents.is_empty());
    }
}
