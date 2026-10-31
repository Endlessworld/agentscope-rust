//! The reporting bridge, mirroring `io.agentscope.extensions.controlplane.SessionBridge`.
//!
//! [`ControlPlaneBridge::start`] assembles the three channels Java `SessionBridge.start`
//! opens: the in-process `/agentscope/*` contract HTTP server (`start_http`),
//! self-registration plus a 15s heartbeat (`start_http_register`), and the ASDP upstream
//! Level-2 outbox pump (`start_grpc` / a host-attached
//! [`AsdpLink`](crate::transport::AsdpLink)). [`ControlPlaneBridge::close`] stops every
//! channel again and detaches the adapter.
//!
//! Level-1/4 upstream push reporting is deliberately not implemented (tracker CP-13): the
//! control plane pulls those over the contract surface.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use agentscope_core::session::SessionExportSink;

use crate::adapter::{
    CommandOutcome, FrameworkAdapter, CAP_SESSION_COMMAND, COMMAND_ABORT, COMMAND_COMPRESS,
    COMMAND_TERMINATE,
};
use crate::committed_export::committed_event_sink;
use crate::config::ControlPlaneConfig;
use crate::error::{ControlPlaneError, ControlPlaneResult};
use crate::event_journal::EventJournal;
use crate::model::{
    ContextSnapshot, ContextTracker, Inventory, MessagePage, SessionEvent, ToolInfo,
};
use crate::transport::{
    AsdpEvent, AsdpTransport, ContractHttpServer, ContractProvider, EventReport, EventReportAck,
    HttpSelfRegistration, RegisteredIdentity, RegistrationOptions, SessionEventMsg,
};

/// Level-2 outbox flush period (`SessionBridge.EVENT_FLUSH_INTERVAL_MS`).
pub const EVENT_FLUSH_INTERVAL: Duration = Duration::from_millis(5_000);
/// Maximum events per `EventReport` (`SessionBridge.EVENT_BATCH_SIZE`).
pub const EVENT_BATCH_SIZE: usize = 20;
/// In-flight ack timeout after which a report is resent (`SessionBridge.EVENT_ACK_TIMEOUT_MS`).
pub const EVENT_ACK_TIMEOUT: Duration = Duration::from_millis(15_000);
/// Registration heartbeat period (`SessionBridge.java:213` passes `15_000L`).
pub const REGISTRATION_HEARTBEAT: Duration = Duration::from_millis(15_000);

/// Test-visible clock so the in-flight ack timeout never needs a real wait.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

fn system_clock() -> Clock {
    Arc::new(Instant::now)
}

/// One `EventReport` that has been sent but not yet acknowledged.
#[derive(Debug, Clone)]
struct InflightReport {
    report: EventReport,
    sent_at: Instant,
}

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
    /// The report currently on the wire, awaiting its `EventReportAck`.
    inflight: Option<InflightReport>,
}

impl BridgeState {
    /// Assigns the next per-session sequence, journals the event and folds it into the
    /// tracker. Shared by [`ControlPlaneBridge`] and [`BridgeReporter`].
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

    /// Ingests one native committed fact, deduplicating by its per-source watermark.
    ///
    /// Mirrors Java `SessionBridge.onCommittedEvent`: a fact at or below the durable
    /// watermark has already been journaled, so re-delivery is a no-op. Returns whether the
    /// fact was newly accepted. Without a journal there is nowhere durable to record the
    /// watermark, so the fact is dropped rather than journaled un-deduplicated (Java throws
    /// here; this path stays non-fatal because reporting must never break the conversation).
    fn publish_committed(
        &mut self,
        agent_key: &str,
        source: &str,
        native_seq: i64,
        event: SessionEvent,
    ) -> bool {
        let Some(journal) = self.journal.as_ref() else {
            return false;
        };
        if journal.source_watermark(source) >= native_seq {
            return false;
        }
        self.publish(agent_key, event);
        true
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

    /// Oldest un-acknowledged events, capped at `max` (Java `EVENT_BATCH_SIZE`).
    fn pending_batch(&self, max: usize) -> Vec<SessionEvent> {
        self.journal
            .as_ref()
            .map(|journal| journal.pending().iter().take(max).cloned().collect())
            .unwrap_or_default()
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
        self.descriptions
            .insert(session_id.to_string(), desc.clone());
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
        self.context_used
            .insert(session_id.to_string(), tokens.max(0));
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

    /// Ingests one native committed fact into the durable journal.
    ///
    /// `source` is the native session the fact belongs to and `native_seq` its committed
    /// sequence. The durable watermark makes re-delivery idempotent. Returns `true` when the
    /// fact was newly journaled; `false` when it was a duplicate or no journal is open.
    pub fn publish_committed(&self, source: &str, native_seq: i64, event: SessionEvent) -> bool {
        self.lock_state()
            .publish_committed(&self.agent_key, source, native_seq, event)
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
        self.lock_state()
            .set_context_used(&self.agent_key, session_id, tokens);
    }
}

/// Reporting bridge for one instrumented agent.
///
/// Mirrors the Java `SessionBridge`: `new` -> [`attach`](Self::attach) ->
/// [`start`](Self::start), and [`close`](Self::close) to detach and release channels.
pub struct ControlPlaneBridge {
    config: ControlPlaneConfig,
    adapter: Option<Arc<dyn FrameworkAdapter>>,
    state: Arc<Mutex<BridgeState>>,
    started: AtomicBool,
    /// The `/agentscope/*` contract server, when `start_http` opened it.
    http: Mutex<Option<ContractHttpServer>>,
    /// Self-registration + heartbeat loop, when `start_http_register` opened it.
    registration: Mutex<Option<Arc<HttpSelfRegistration>>>,
    /// The ASDP upstream channel, when the host attached one.
    asdp: Mutex<Option<Arc<AsdpTransport>>>,
    /// Background tasks (outbox pump + downstream reader) owned by the bridge.
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Clock used by the in-flight ack timeout.
    clock: Clock,
}

impl ControlPlaneBridge {
    /// Creates a bridge without attaching or starting it, mirroring `ControlPlane.newBridge`.
    pub fn new(config: ControlPlaneConfig) -> Self {
        Self {
            config,
            adapter: None,
            state: Arc::new(Mutex::new(BridgeState::default())),
            started: AtomicBool::new(false),
            http: Mutex::new(None),
            registration: Mutex::new(None),
            asdp: Mutex::new(None),
            tasks: Mutex::new(Vec::new()),
            clock: system_clock(),
        }
    }

    /// Overrides the clock used by the in-flight ack timeout (tests use a fake one so no
    /// real 15s wait is needed).
    #[doc(hidden)]
    pub fn set_clock(&mut self, clock: Clock) {
        self.clock = clock;
    }

    /// Locks the shared bridge state, recovering from poisoning (a failed report must
    /// never take the bridge down).
    fn lock_state(&self) -> MutexGuard<'_, BridgeState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The bridge config.
    pub fn config(&self) -> &ControlPlaneConfig {
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
    ) -> ControlPlaneResult<()> {
        if !adapter.can_handle(target.as_ref()) {
            return Err(crate::error::ControlPlaneError::Config(
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
        let emit: Arc<dyn Fn(SessionEvent) + Send + Sync> = Arc::new(move |event: SessionEvent| {
            emit_reporter.publish(event);
        });
        adapter.attach(target, emit)?;
        adapter.on_bridge_attached(reporter);
        // The contract server's provider shares the adapter with the bridge, so commands
        // arriving over HTTP reach the same instance the bridge mounted.
        self.adapter = Some(Arc::from(adapter));
        Ok(())
    }

    /// Starts every channel the config enables: the `/agentscope/*` contract server, HTTP
    /// self-registration (with a 15s heartbeat) and the ASDP Level-2 outbox pump.
    ///
    /// Mirrors `SessionBridge.start`. Registration failures are swallowed — reporting must
    /// never disturb the agent — but a contract-server bind failure is returned so a
    /// misconfigured port surfaces immediately.
    pub async fn start(mut self) -> ControlPlaneResult<Self> {
        if self.config.enable_events && self.lock_state().journal.is_none() {
            let journal = EventJournal::new(&self.config)?;
            self.lock_state().journal = Some(journal);
        }
        if self.config.start_http {
            let provider: Arc<dyn ContractProvider> = Arc::new(self.contract_view());
            let mut server = ContractHttpServer::bind(
                &self.config.contract_http_host,
                self.config.contract_http_port,
                provider,
                &self.config.internal_token,
            )
            .await?;
            server.start()?;
            *self.http.lock().unwrap_or_else(|p| p.into_inner()) = Some(server);
        }
        if self.config.start_http_register {
            let registration = Arc::new(HttpSelfRegistration::new(
                &self.config.control_plane_http,
                &self.config.internal_token,
                self.registration_options()?,
            )?);
            // Registers once, then heartbeats every 15s; failures are retried in the loop.
            registration.start().await;
            *self.registration.lock().unwrap_or_else(|p| p.into_inner()) = Some(registration);
        }
        let asdp = self.asdp();
        match asdp {
            Some(transport) => self.spawn_event_pump(transport, EVENT_FLUSH_INTERVAL),
            None if self.config.start_grpc => tracing::warn!(
                "controlplane: start_grpc is set but no AsdpLink was attached; \
                 the ASDP wire layer is host-provided"
            ),
            None => {}
        }
        self.started.store(true, Ordering::SeqCst);
        Ok(self)
    }

    /// The port the `/agentscope/*` contract server is bound to, when it is running.
    ///
    /// With `contract_http_port(0)` this is the ephemeral port the OS assigned.
    pub fn contract_port(&self) -> Option<u16> {
        self.http
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|server| server.port())
    }

    /// The identity returned by the last successful registration, when self-registration is
    /// enabled and the control plane accepted this instance.
    pub fn registered_identity(&self) -> Option<RegisteredIdentity> {
        self.registration
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .and_then(|registration| registration.identity())
    }

    /// The registration request this bridge would send (also the body asserted against the
    /// control plane's `POST /api/v1/agent-registrations`).
    pub fn registration_options(&self) -> ControlPlaneResult<RegistrationOptions> {
        let capabilities = self
            .adapter
            .as_ref()
            .map(|adapter| adapter.capabilities())
            .unwrap_or_default();
        RegistrationOptions::new(
            self.config.agent_key.clone(),
            self.config.instance_key.clone(),
        )
        .base_url(self.contract_base_url())
        .framework(
            self.adapter
                .as_ref()
                .map(|adapter| adapter.framework_name().to_string())
                .unwrap_or_default(),
        )
        .runtime(env!("CARGO_PKG_VERSION"))
        .capabilities(capabilities)
        .tenant(self.config.tenant.clone())
        .namespace(self.config.namespace.clone())
        .registration_credential(self.config.registration_credential.clone())
        .heartbeat_interval(REGISTRATION_HEARTBEAT)
        .validate()
    }

    /// The URL the control plane should use to reach this instance's contract surface.
    fn contract_base_url(&self) -> String {
        if !self.config.public_base_url.is_empty() {
            return self.config.public_base_url.clone();
        }
        let port = self
            .contract_port()
            .unwrap_or(self.config.contract_http_port);
        let host = self.config.contract_http_host.trim();
        let host = if host.is_empty() { "127.0.0.1" } else { host };
        format!("http://{host}:{port}")
    }

    /// Attaches the ASDP upstream channel.
    ///
    /// The wire layer is host-provided (tracker CP-16): call this before
    /// [`start`](Self::start) so the Level-2 outbox pump and the downstream reader are
    /// spawned with the bridge.
    pub fn attach_asdp(&mut self, transport: Arc<AsdpTransport>) {
        *self.asdp.lock().unwrap_or_else(|p| p.into_inner()) = Some(transport);
    }

    /// The attached ASDP channel, when any.
    pub fn asdp(&self) -> Option<Arc<AsdpTransport>> {
        self.asdp
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Whether Level-2 events can leave the durable outbox.
    pub fn has_asdp_link(&self) -> bool {
        self.asdp().is_some()
    }

    /// Sends one batch of pending Level-2 events over the ASDP channel.
    ///
    /// Returns how many events left the outbox. Mirrors `SessionBridge.flushEvents`:
    /// * at most [`EVENT_BATCH_SIZE`] events per report,
    /// * a report already in flight blocks further flushing until it is acknowledged or
    ///   [`EVENT_ACK_TIMEOUT`] elapses, after which the **same** `report_id` is resent (the
    ///   control plane deduplicates on it),
    /// * without a link nothing is sent (the journal keeps growing; see tracker CP-04).
    pub async fn flush_events(&self) -> ControlPlaneResult<usize> {
        let Some(transport) = self.asdp() else {
            return Ok(0);
        };
        flush_once(&self.state, transport.as_ref(), (self.clock)()).await
    }

    /// Applies one ASDP `EventAck` (mirrors `SessionBridge.onEventAck`).
    ///
    /// An empty `error` commits the cursors and releases the in-flight report; a non-empty
    /// `error` keeps it so the pump replays the same `report_id` after the ack timeout.
    pub fn on_event_ack(&self, ack: &EventReportAck) -> ControlPlaneResult<()> {
        apply_ack(&self.state, ack)
    }

    /// Spawns the outbox pump and the downstream reader.
    ///
    /// `interval` is the flush period ([`EVENT_FLUSH_INTERVAL`] in production, a few
    /// milliseconds in tests so no real 5s wait is needed).
    fn spawn_event_pump(&mut self, transport: Arc<AsdpTransport>, interval: Duration) {
        let state = Arc::clone(&self.state);
        let clock = Arc::clone(&self.clock);
        let pump_transport = Arc::clone(&transport);
        let pump = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // the first tick fires immediately
            loop {
                ticker.tick().await;
                let now = clock();
                if let Err(error) = flush_once(&state, &pump_transport, now).await {
                    tracing::debug!("controlplane: event flush failed: {error}");
                }
            }
        });
        let read_state = Arc::clone(&self.state);
        let reader = tokio::spawn(async move {
            loop {
                match transport.next_event().await {
                    Ok(Some(AsdpEvent::Acked(ack))) => {
                        if let Err(error) = apply_ack(&read_state, &ack) {
                            tracing::debug!("controlplane: event ack rejected: {error}");
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
                }
            }
        });
        let mut tasks = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
        tasks.push(pump);
        tasks.push(reader);
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

    /// A committed-event sink wired to this bridge's journal.
    ///
    /// Install it on an agent's runtime context under
    /// [`SESSION_EXPORT_SINK_CONTEXT_KEY`](agentscope_core::session::SESSION_EXPORT_SINK_CONTEXT_KEY)
    /// (wrapped in a `SessionExportSinkHandle`) to feed native committed facts into the
    /// Level-2 stream. Mirrors the sink Java's observer middleware installs.
    pub fn committed_event_sink(&self) -> Arc<dyn SessionExportSink> {
        committed_event_sink(self.reporter())
    }

    /// The committed-event sink already wrapped for a runtime-context extension bag.
    ///
    /// Convenience so a host can write
    /// `.extension(SESSION_EXPORT_SINK_CONTEXT_KEY, bridge.export_sink_handle())`.
    pub fn export_sink_handle(&self) -> agentscope_core::session::SessionExportSinkHandle {
        agentscope_core::session::SessionExportSinkHandle::new(self.committed_event_sink())
    }

    /// Number of un-acknowledged Level-2 events in the durable outbox.
    pub fn pending_event_count(&self) -> usize {
        self.lock_state()
            .journal
            .as_ref()
            .map(|journal| journal.pending_len())
            .unwrap_or(0)
    }

    /// Snapshot of the un-acknowledged Level-2 events.
    pub fn pending_events(&self) -> Vec<SessionEvent> {
        self.lock_state()
            .journal
            .as_ref()
            .map(|journal| journal.pending().to_vec())
            .unwrap_or_default()
    }

    /// Drops the pending events the control plane has committed, per session.
    ///
    /// `committed` maps `sessionId -> last committed seq` (Java `EventReportAck.committed`,
    /// folded with `Math::max` per session). Mirrors `SessionBridge.onEventAck` →
    /// `EventJournal.acknowledge`, including its “keep the batch and replay” failure mode:
    /// an empty map is a no-op and a missing journal (reporting disabled) acknowledges
    /// nothing. The control plane is idempotent on `(session, seq)`, so a replay after a
    /// failed checkpoint is always safe.
    pub fn acknowledge(&self, committed: &BTreeMap<String, i64>) -> ControlPlaneResult<()> {
        match self.lock_state().journal.as_mut() {
            Some(journal) => journal.acknowledge(committed),
            None => Ok(()),
        }
    }

    /// Durable native watermark for `source` (0 when none).
    pub fn source_watermark(&self, source: &str) -> i64 {
        self.lock_state()
            .journal
            .as_ref()
            .map(|journal| journal.source_watermark(source))
            .unwrap_or(0)
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
        self.lock_state()
            .set_context_used(&agent_key, session_id, tokens);
    }

    /// Whether `session_id` has been seen by this bridge.
    pub fn is_known_session(&self, session_id: &str) -> bool {
        self.lock_state().trackers.contains_key(session_id)
    }

    /// Detaches the adapter and releases channels, mirroring `SessionBridge.close`.
    ///
    /// The contract server stops accepting immediately; the ASDP channel close and the
    /// registration delete are fired on the current runtime when there is one (a bridge may
    /// legitimately be closed from a non-async context).
    pub fn close(&mut self) -> ControlPlaneResult<()> {
        if let Some(mut server) = self.http.lock().unwrap_or_else(|p| p.into_inner()).take() {
            server.stop();
        }
        {
            let mut tasks = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
            for task in tasks.drain(..) {
                task.abort();
            }
        }
        if let Some(transport) = self.asdp.lock().unwrap_or_else(|p| p.into_inner()).take() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = transport.close().await;
                });
            }
        }
        if let Some(registration) = self
            .registration
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = registration.unregister().await;
                });
            }
        }
        if let Some(adapter) = self.adapter.take() {
            let _ = adapter.detach();
        }
        self.lock_state().inflight = None;
        if let Some(journal) = self.lock_state().journal.as_mut() {
            journal.ack_all();
        }
        self.started.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Materializes the Level-4 context snapshot for a session.
    pub fn context_snapshot(&self, session_id: &str) -> ControlPlaneResult<ContextSnapshot> {
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

impl ControlPlaneBridge {
    /// A contract view over this bridge: shared event state plus the mounted adapter.
    fn contract_view(&self) -> BridgeContractView {
        BridgeContractView {
            config: self.config.clone(),
            state: Arc::clone(&self.state),
            adapter: self.adapter.clone(),
        }
    }
}

/// Contract view over a bridge's shared state and mounted adapter.
///
/// The HTTP server needs an `Arc<dyn ContractProvider>` that outlives any borrow of the
/// bridge, so the provider is this cheap snapshot: the state is shared and the adapter
/// handle is cloned. Keeping one type in front of both sources is what lets the command
/// routes dispatch to the framework instead of falling back to the trait defaults.
struct BridgeContractView {
    config: ControlPlaneConfig,
    state: Arc<Mutex<BridgeState>>,
    adapter: Option<Arc<dyn FrameworkAdapter>>,
}

impl BridgeContractView {
    fn lock_state(&self) -> MutexGuard<'_, BridgeState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The adapter, or the capability name that explains why the route cannot work.
    fn require_adapter(&self, capability: &str) -> ControlPlaneResult<Arc<dyn FrameworkAdapter>> {
        self.adapter
            .clone()
            .ok_or_else(|| ControlPlaneError::Unsupported(capability.to_string()))
    }

    /// Dispatches a session command to the adapter.
    ///
    /// A missing adapter or an unsupported command is an error, never a silent success
    /// (CP-02): `terminate` / `abort` used to acknowledge themselves without doing anything.
    async fn dispatch_command(
        &self,
        session_id: &str,
        command: &str,
    ) -> ControlPlaneResult<CommandOutcome> {
        let adapter = self.require_adapter(CAP_SESSION_COMMAND)?;
        let outcome = adapter.execute_command(session_id, command, None).await?;
        if !outcome.ok {
            let message = if outcome.message.is_empty() {
                format!("session {session_id} is not active")
            } else {
                outcome.message
            };
            return Err(ControlPlaneError::NotFound(message));
        }
        Ok(outcome)
    }

    fn is_busy(&self, session_id: &str) -> bool {
        self.lock_state().busy.contains(session_id)
    }
}

#[async_trait]
impl ContractProvider for BridgeContractView {
    async fn inventory(&self) -> ControlPlaneResult<Inventory> {
        Ok(Inventory::health_only(self.lock_state().trackers.len() as i64))
    }

    async fn context(&self, session_id: &str) -> ControlPlaneResult<ContextSnapshot> {
        let framework = self.config.agent_key.clone();
        let state = self.lock_state();
        match state.trackers.get(session_id) {
            Some(tracker) => Ok(tracker.snapshot()),
            None => Ok(ContextSnapshot::builder(session_id)
                .framework(framework)
                .build()),
        }
    }

    async fn messages(
        &self,
        session_id: &str,
        offset: i64,
        limit: i64,
    ) -> ControlPlaneResult<MessagePage> {
        // Level-3 full history is served by the adapter when it declares `message-query`.
        let items = match self.adapter.clone() {
            Some(adapter) => adapter
                .list_messages(session_id, offset, limit)
                .await
                .map(|page| page.messages)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        Ok(MessagePage::of(session_id, items, offset, limit))
    }

    async fn session_ids(&self) -> ControlPlaneResult<Vec<String>> {
        let from_events: Vec<String> = self.lock_state().trackers.keys().cloned().collect();
        Ok(from_events)
    }

    async fn info(&self) -> ControlPlaneResult<serde_json::Value> {
        let adapter = self.adapter.clone();
        let mut cfg = adapter
            .as_ref()
            .map(|adapter| adapter.build_agent_config())
            .unwrap_or_default();
        cfg.insert("name".into(), self.config.agent_key.clone().into());
        cfg.insert("sources".into(), serde_json::json!(["bridge"]));
        Ok(serde_json::json!({
            "agentKey": self.config.agent_key,
            "tenant": self.config.tenant,
            "namespace": self.config.namespace,
            "instanceKey": self.config.instance_key,
            "framework": adapter.as_ref().map(|a| a.framework_name()).unwrap_or(""),
            "frameworkVersion": adapter.as_ref().map(|a| a.framework_version()).unwrap_or(""),
            "capabilities": adapter.as_ref().map(|a| a.capabilities()).unwrap_or_default(),
            "agentConfig": cfg,
        }))
    }

    async fn subagents(&self) -> ControlPlaneResult<Vec<serde_json::Value>> {
        let inv = self.inventory().await?;
        Ok(inv.subagents.iter().map(|s| s.to_json_value()).collect())
    }

    async fn workspaces(&self) -> ControlPlaneResult<Vec<serde_json::Value>> {
        let inv = self.inventory().await?;
        Ok(inv
            .workspaces
            .iter()
            .map(|w| serde_json::json!({ "path": w.path, "mode": w.mode }))
            .collect())
    }

    /// `GET .../tasks`: the adapter's todo list (capability `task-query`).
    async fn tasks(&self, session_id: &str) -> ControlPlaneResult<serde_json::Value> {
        let adapter = self.require_adapter(crate::adapter::CAP_TASK_QUERY)?;
        let items = adapter.list_tasks(session_id).await?;
        Ok(serde_json::json!({ "tasks": items }))
    }

    /// `GET .../subagent-tasks` (capability `subagent-task-query`).
    async fn subagent_tasks(&self, session_id: &str) -> ControlPlaneResult<serde_json::Value> {
        let adapter = self.require_adapter(crate::adapter::CAP_SUBAGENT_TASK_QUERY)?;
        let items = adapter.list_subagent_tasks(session_id).await?;
        Ok(serde_json::json!({ "tasks": items }))
    }

    /// `DELETE .../subagent-tasks/{taskId}` (capability `subagent-task-command`).
    async fn cancel_subagent_task(&self, session_id: &str, task_id: &str) -> ControlPlaneResult<()> {
        let adapter = self.require_adapter(crate::adapter::CAP_SUBAGENT_TASK_COMMAND)?;
        adapter.cancel_subagent_task(session_id, task_id).await
    }

    /// `POST .../compress`: a busy session is a 409, otherwise the adapter compacts it.
    async fn compress(&self, session_id: &str) -> ControlPlaneResult<()> {
        if self.is_busy(session_id) {
            return Err(ControlPlaneError::Busy("session is busy".into()));
        }
        self.dispatch_command(session_id, COMMAND_COMPRESS)
            .await
            .map(|_| ())
    }

    /// `POST .../terminate`: interrupts the session through the adapter.
    async fn terminate(&self, session_id: &str) -> ControlPlaneResult<()> {
        self.dispatch_command(session_id, COMMAND_TERMINATE)
            .await
            .map(|_| ())
    }

    /// `POST .../abort`: aborts the current turn without ending the session.
    async fn abort(&self, session_id: &str) -> ControlPlaneResult<()> {
        self.dispatch_command(session_id, COMMAND_ABORT).await?;
        self.lock_state().busy.remove(session_id);
        Ok(())
    }

    /// `POST .../plan-mode`: body is `{"active": bool}` (capability `plan-mode`).
    async fn plan_mode(&self, session_id: &str, body: serde_json::Value) -> ControlPlaneResult<()> {
        let adapter = self.require_adapter(crate::adapter::CAP_PLAN_MODE)?;
        adapter.set_plan_mode(session_id, body).await
    }

    /// `POST .../messages`: injects a console message (capability `conversation-inbound`).
    async fn post_message(&self, session_id: &str, body: serde_json::Value) -> ControlPlaneResult<()> {
        if self.is_busy(session_id) {
            return Err(ControlPlaneError::Busy("session is busy".into()));
        }
        let content = message_content(&body);
        if content.trim().is_empty() {
            return Err(ControlPlaneError::Config("content is required".into()));
        }
        let adapter = self.require_adapter(crate::adapter::CAP_CONVERSATION_INBOUND)?;
        adapter.inject_user_message(session_id, &content).await
    }

    /// `GET .../export-transcript`: the full history in a downloadable envelope.
    async fn export_transcript(&self, session_id: &str) -> ControlPlaneResult<serde_json::Value> {
        let page = ContractProvider::messages(self, session_id, 0, i64::MAX).await?;
        Ok(serde_json::json!({
            "sessionId": session_id,
            "format": "json",
            "messages": page.messages.iter().map(|item| item.to_json_value()).collect::<Vec<_>>(),
        }))
    }
}

/// Reads the injected message text, mirroring `SessionBridge.readNotice`.
fn message_content(body: &serde_json::Value) -> String {
    let text_of = |key: &str| body.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let content = text_of("content");
    if !content.is_empty() {
        return content;
    }
    let message = text_of("message");
    if !message.is_empty() {
        return message;
    }
    match body {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Object(_) => body.to_string(),
        _ => String::new(),
    }
}

/// Maps a journaled Level-2 event onto its ASDP wire shape.
fn session_event_msg(event: &SessionEvent) -> SessionEventMsg {
    SessionEventMsg {
        session_id: event.session_id.clone(),
        seq: event.seq as i32,
        event_type: event.event_type.clone(),
        occurred_at: event.occurred_at,
        role: event.role.clone(),
        content: event.content.clone(),
        tool_name: event.tool_name.clone(),
        tool_input: event
            .tool_input
            .as_ref()
            .map(|value| value.to_string()),
        tool_output: event.tool_output.clone(),
        tokens_in: event.tokens_in as i32,
        tokens_out: event.tokens_out as i32,
        duration_ms: event.duration_ms as i32,
        framework_meta: event
            .framework_meta
            .as_ref()
            .map(|value| value.to_string()),
    }
}

/// One flush step over the shared state (used by [`ControlPlaneBridge::flush_events`] and
/// by the background pump).
async fn flush_once(
    state: &Arc<Mutex<BridgeState>>,
    transport: &AsdpTransport,
    now: Instant,
) -> ControlPlaneResult<usize> {
    let report = {
        let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
        match guard.inflight.clone() {
            Some(inflight) => {
                if now.saturating_duration_since(inflight.sent_at) < EVENT_ACK_TIMEOUT {
                    return Ok(0);
                }
                guard.inflight = Some(InflightReport {
                    report: inflight.report.clone(),
                    sent_at: now,
                });
                inflight.report
            }
            None => {
                let batch = guard.pending_batch(EVENT_BATCH_SIZE);
                if batch.is_empty() {
                    return Ok(0);
                }
                let report = EventReport {
                    events: batch.iter().map(session_event_msg).collect(),
                    report_id: uuid::Uuid::new_v4().to_string(),
                };
                guard.inflight = Some(InflightReport {
                    report: report.clone(),
                    sent_at: now,
                });
                report
            }
        }
    };
    let count = report.events.len();
    if !transport.report_events(report).await? {
        state.lock().unwrap_or_else(|p| p.into_inner()).inflight = None;
        return Ok(0);
    }
    Ok(count)
}

/// Applies one ack to the shared state (used by the background reader).
fn apply_ack(state: &Arc<Mutex<BridgeState>>, ack: &EventReportAck) -> ControlPlaneResult<()> {
    if !ack.error.is_empty() {
        return Err(ControlPlaneError::ControlPlane {
            status: 0,
            message: format!("event report rejected: {}", ack.error),
        });
    }
    let committed: BTreeMap<String, i64> = ack
        .committed
        .iter()
        .map(|cursor| (cursor.session_id.clone(), cursor.committed_seq as i64))
        .collect();
    let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(journal) = guard.journal.as_mut() {
        journal.acknowledge(&committed)?;
    }
    let matches = guard
        .inflight
        .as_ref()
        .map(|inflight| inflight.report.report_id == ack.report_id)
        .unwrap_or(false);
    if matches {
        guard.inflight = None;
    }
    Ok(())
}

/// Contract provider for the bridge itself: every route delegates to the same view the
/// HTTP server uses, so an in-process caller and the wire see identical behavior.
#[async_trait]
impl ContractProvider for ControlPlaneBridge {
    async fn inventory(&self) -> ControlPlaneResult<Inventory> {
        self.contract_view().inventory().await
    }

    async fn context(&self, session_id: &str) -> ControlPlaneResult<ContextSnapshot> {
        self.contract_view().context(session_id).await
    }

    async fn messages(
        &self,
        session_id: &str,
        offset: i64,
        limit: i64,
    ) -> ControlPlaneResult<MessagePage> {
        self.contract_view()
            .messages(session_id, offset, limit)
            .await
    }

    async fn session_ids(&self) -> ControlPlaneResult<Vec<String>> {
        self.contract_view().session_ids().await
    }

    async fn info(&self) -> ControlPlaneResult<serde_json::Value> {
        self.contract_view().info().await
    }

    async fn subagents(&self) -> ControlPlaneResult<Vec<serde_json::Value>> {
        self.contract_view().subagents().await
    }

    async fn workspaces(&self) -> ControlPlaneResult<Vec<serde_json::Value>> {
        self.contract_view().workspaces().await
    }

    async fn tasks(&self, session_id: &str) -> ControlPlaneResult<serde_json::Value> {
        self.contract_view().tasks(session_id).await
    }

    async fn subagent_tasks(&self, session_id: &str) -> ControlPlaneResult<serde_json::Value> {
        self.contract_view().subagent_tasks(session_id).await
    }

    async fn cancel_subagent_task(&self, session_id: &str, task_id: &str) -> ControlPlaneResult<()> {
        self.contract_view().cancel_subagent_task(session_id, task_id).await
    }

    async fn compress(&self, session_id: &str) -> ControlPlaneResult<()> {
        self.contract_view().compress(session_id).await
    }

    async fn terminate(&self, session_id: &str) -> ControlPlaneResult<()> {
        self.contract_view().terminate(session_id).await
    }

    async fn abort(&self, session_id: &str) -> ControlPlaneResult<()> {
        self.contract_view().abort(session_id).await
    }

    async fn plan_mode(&self, session_id: &str, body: serde_json::Value) -> ControlPlaneResult<()> {
        self.contract_view().plan_mode(session_id, body).await
    }

    async fn post_message(&self, session_id: &str, body: serde_json::Value) -> ControlPlaneResult<()> {
        self.contract_view().post_message(session_id, body).await
    }

    async fn export_transcript(&self, session_id: &str) -> ControlPlaneResult<serde_json::Value> {
        self.contract_view().export_transcript(session_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::session_event::TYPE_MESSAGE;
    use crate::transport::{AsdpLink, ConnectRequest, SessionEventCursor, UpstreamMeta};
    use std::sync::atomic::AtomicU64;

    fn events_config() -> ControlPlaneConfig {
        ControlPlaneConfig::builder("friday")
            .instance_key("rep-1")
            .enable_events(true)
            .event_journal_dir(
                std::env::temp_dir()
                    .join(format!("controlplane-bridge-{}", uuid::Uuid::new_v4()))
                    .to_str()
                    .unwrap(),
            )
            .build()
            .unwrap()
    }

    async fn temp_bridge() -> ControlPlaneBridge {
        ControlPlaneBridge::new(events_config()).start().await.unwrap()
    }

    /// A started bridge with an open event journal but no HTTP server and no self-registration.
    ///
    /// The outbox lives in the journal, which only `start()` opens, and the flush tests drive
    /// `flush_events` by hand: nothing else may touch the outbox or the port space.
    async fn outbox_bridge() -> ControlPlaneBridge {
        let config = ControlPlaneConfig::builder("friday")
            .instance_key("rep-1")
            .enable_events(true)
            .event_journal_dir(
                std::env::temp_dir()
                    .join(format!("controlplane-outbox-{}", uuid::Uuid::new_v4()))
                    .to_str()
                    .unwrap(),
            )
            .start_http(false)
            .start_http_register(false)
            .build()
            .unwrap();
        ControlPlaneBridge::new(config).start().await.unwrap()
    }

    #[tokio::test]
    async fn record_event_assigns_monotonic_seq() {
        let bridge = temp_bridge().await;
        let s1 = bridge.record_event(
            SessionEvent::builder("s1", TYPE_MESSAGE)
                .content("a")
                .build(),
        );
        let s2 = bridge.record_event(
            SessionEvent::builder("s1", TYPE_MESSAGE)
                .content("b")
                .build(),
        );
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
        assert_eq!(bridge.tracker("s1").effective_message_count(), 2);
    }

    #[tokio::test]
    async fn describe_session_seeds_tracker_metadata() {
        let bridge = temp_bridge().await;
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

    #[tokio::test]
    async fn busy_and_effective_prompt_round_trip() {
        let bridge = temp_bridge().await;
        assert!(!bridge.is_busy("s1"));
        bridge.set_busy("s1", true);
        assert!(bridge.is_busy("s1"));
        bridge.set_busy("s1", false);
        assert!(!bridge.is_busy("s1"));
        bridge.set_effective_system_prompt("s1", "composed");
        assert_eq!(bridge.effective_system_prompt("s1"), "composed");
        assert_eq!(bridge.effective_system_prompt("missing"), "");
    }

    #[tokio::test]
    async fn set_context_used_tokens_updates_snapshot() {
        let bridge = temp_bridge().await;
        bridge.set_context_used_tokens("s1", 4096);
        let snap = bridge.context_snapshot("s1").unwrap();
        assert_eq!(snap.total_tokens, 4096);
        assert!(bridge.is_known_session("s1"));
        assert!(!bridge.is_known_session("nope"));
    }

    #[tokio::test]
    async fn contract_provider_reports_empty_inventory() {
        let cfg = ControlPlaneConfig::builder("friday")
            .instance_key("rep-1")
            .build()
            .unwrap();
        let bridge = ControlPlaneBridge::new(cfg);
        let inv = bridge.inventory().await.unwrap();
        assert!(inv.subagents.is_empty());
    }

    // ─── CP-01: startup assembly ───

    /// A control-plane stub that records `POST /api/v1/agent-registrations` bodies.
    async fn registration_stub() -> (Arc<Mutex<Vec<serde_json::Value>>>, u16) {
        use axum::{extract::State, routing::post, Json, Router};

        type Recorded = Arc<Mutex<Vec<serde_json::Value>>>;
        async fn register(
            State(recorded): State<Recorded>,
            Json(body): Json<serde_json::Value>,
        ) -> Json<serde_json::Value> {
            recorded.lock().unwrap().push(body);
            Json(serde_json::json!({
                "agent": { "id": "agent-1" },
                "binding": { "id": "binding-1" },
                "instance": { "id": "instance-1", "generation": 3 },
                "registrationCredential": "cred-1",
            }))
        }

        let recorded: Recorded = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/api/v1/agent-registrations", post(register))
            .with_state(Arc::clone(&recorded));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (recorded, port)
    }

    /// CP-01: `start` must bind the `/agentscope/*` contract surface and self-register with
    /// the fields the config describes. Before this change `start` only opened the journal,
    /// so `contract_port()` was always `None` and nothing ever reached the control plane.
    #[tokio::test]
    async fn start_binds_the_contract_surface_and_registers_with_config_fields() {
        let (recorded, registration_port) = registration_stub().await;
        let mut cfg = ControlPlaneConfig::builder("friday")
            .instance_key("rep-1")
            .tenant("acme")
            .namespace("ns-1")
            .internal_token("tok")
            .control_plane_http(format!("http://127.0.0.1:{registration_port}"))
            .contract_http_host("127.0.0.1")
            .contract_http_port(0)
            .build()
            .unwrap();
        cfg.start_http = true;
        cfg.start_http_register = true;

        let mut bridge = ControlPlaneBridge::new(cfg).start().await.unwrap();

        let port = bridge.contract_port().expect("contract server was not started");
        assert_ne!(port, 0, "contract_http_port(0) must bind an ephemeral port");
        let health: serde_json::Value = reqwest::get(format!(
            "http://127.0.0.1:{port}/agentscope/health"
        ))
        .await
        .expect("health request")
        .json()
        .await
        .expect("health body");
        assert_eq!(health["status"], "ok");

        let bodies = recorded.lock().unwrap().clone();
        assert_eq!(bodies.len(), 1, "expected exactly one registration attempt");
        let body = &bodies[0];
        assert_eq!(body["agentKey"], "friday");
        assert_eq!(body["instanceKey"], "rep-1");
        assert_eq!(body["tenant"], "acme");
        assert_eq!(body["namespace"], "ns-1");
        assert_eq!(body["routingKey"], format!("http://127.0.0.1:{port}"));
        assert_eq!(body["sdkVersion"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["capacity"], 1);

        let identity = bridge.registered_identity().expect("identity not recorded");
        assert_eq!(identity.instance_id, "instance-1");
        assert_eq!(identity.generation, 3);
        assert_eq!(identity.registration_credential, "cred-1");

        bridge.close().unwrap();
        assert!(bridge.contract_port().is_none());
        assert!(!bridge.is_started());
    }

    /// CP-01: the heartbeat period is the Java 15s, not the registration client's 30s
    /// default.
    #[tokio::test]
    async fn registration_options_use_the_java_heartbeat_and_identity() {
        let cfg = ControlPlaneConfig::builder("friday")
            .instance_key("rep-1")
            .public_base_url("https://agent.example/")
            .build()
            .unwrap();
        let bridge = ControlPlaneBridge::new(cfg);
        let options = bridge.registration_options().unwrap();
        assert_eq!(REGISTRATION_HEARTBEAT, Duration::from_millis(15_000));
        assert_eq!(options.heartbeat_interval, REGISTRATION_HEARTBEAT);
        assert_eq!(options.agent_key, "friday");
        assert_eq!(options.instance_key, "rep-1");
        assert_eq!(options.base_url, "https://agent.example");
    }

    // ─── CP-02: contract command routing ───

    /// Adapter double that records what the bridge forwards to it.
    #[derive(Default)]
    struct RecordingAdapter {
        commands: Mutex<Vec<String>>,
        plan_mode: Mutex<Option<serde_json::Value>>,
        injected: Mutex<Vec<String>>,
        cancelled: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl FrameworkAdapter for RecordingAdapter {
        fn framework_name(&self) -> &str {
            "recording"
        }
        fn capabilities(&self) -> Vec<String> {
            vec!["context-query".into()]
        }
        fn can_handle(&self, _target: &(dyn std::any::Any + Send + Sync)) -> bool {
            true
        }
        fn attach(
            &mut self,
            _target: Arc<dyn std::any::Any + Send + Sync>,
            _emit: Arc<dyn Fn(SessionEvent) + Send + Sync>,
        ) -> ControlPlaneResult<()> {
            Ok(())
        }
        fn detach(&self) -> ControlPlaneResult<()> {
            Ok(())
        }
        async fn extract_context(&self, session_id: &str) -> ControlPlaneResult<ContextSnapshot> {
            Ok(ContextSnapshot::builder(session_id)
                .framework("recording")
                .build())
        }
        async fn execute_command(
            &self,
            _session_id: &str,
            command: &str,
            _payload: Option<serde_json::Value>,
        ) -> ControlPlaneResult<CommandOutcome> {
            self.commands.lock().unwrap().push(command.to_string());
            Ok(CommandOutcome {
                ok: true,
                message: String::new(),
            })
        }
        async fn list_tasks(&self, _session_id: &str) -> ControlPlaneResult<Vec<serde_json::Value>> {
            Ok(vec![serde_json::json!({ "id": "t1", "state": "pending" })])
        }
        async fn list_subagent_tasks(
            &self,
            _session_id: &str,
        ) -> ControlPlaneResult<Vec<serde_json::Value>> {
            Ok(vec![serde_json::json!({ "id": "st1", "status": "running" })])
        }
        async fn cancel_subagent_task(
            &self,
            _session_id: &str,
            task_id: &str,
        ) -> ControlPlaneResult<()> {
            self.cancelled.lock().unwrap().push(task_id.to_string());
            Ok(())
        }
        async fn set_plan_mode(
            &self,
            _session_id: &str,
            params: serde_json::Value,
        ) -> ControlPlaneResult<()> {
            *self.plan_mode.lock().unwrap() = Some(params);
            Ok(())
        }
        async fn inject_user_message(
            &self,
            _session_id: &str,
            content: &str,
        ) -> ControlPlaneResult<()> {
            self.injected.lock().unwrap().push(content.to_string());
            Ok(())
        }
    }

    fn adapter_bridge() -> (ControlPlaneBridge, Arc<RecordingAdapter>) {
        let mut bridge = ControlPlaneBridge::new(events_config());
        let adapter = Arc::new(RecordingAdapter::default());
        let target: Arc<dyn std::any::Any + Send + Sync> = Arc::new(0u8);
        // `attach` takes ownership, so mount a proxy that shares the recorded state.
        bridge
            .attach(target, Box::new(SharedRecordingAdapter(Arc::clone(&adapter))))
            .unwrap();
        (bridge, adapter)
    }

    /// `Arc<RecordingAdapter>` cannot be boxed by value, so the bridge mounts a thin proxy.
    struct SharedRecordingAdapter(Arc<RecordingAdapter>);

    #[async_trait]
    impl FrameworkAdapter for SharedRecordingAdapter {
        fn framework_name(&self) -> &str {
            self.0.framework_name()
        }
        fn capabilities(&self) -> Vec<String> {
            self.0.capabilities()
        }
        fn can_handle(&self, target: &(dyn std::any::Any + Send + Sync)) -> bool {
            self.0.can_handle(target)
        }
        fn attach(
            &mut self,
            target: Arc<dyn std::any::Any + Send + Sync>,
            emit: Arc<dyn Fn(SessionEvent) + Send + Sync>,
        ) -> ControlPlaneResult<()> {
            let _ = (target, emit);
            Ok(())
        }
        fn detach(&self) -> ControlPlaneResult<()> {
            self.0.detach()
        }
        async fn extract_context(&self, session_id: &str) -> ControlPlaneResult<ContextSnapshot> {
            self.0.extract_context(session_id).await
        }
        async fn execute_command(
            &self,
            session_id: &str,
            command: &str,
            payload: Option<serde_json::Value>,
        ) -> ControlPlaneResult<CommandOutcome> {
            self.0.execute_command(session_id, command, payload).await
        }
        async fn list_tasks(&self, session_id: &str) -> ControlPlaneResult<Vec<serde_json::Value>> {
            self.0.list_tasks(session_id).await
        }
        async fn list_subagent_tasks(
            &self,
            session_id: &str,
        ) -> ControlPlaneResult<Vec<serde_json::Value>> {
            self.0.list_subagent_tasks(session_id).await
        }
        async fn cancel_subagent_task(
            &self,
            session_id: &str,
            task_id: &str,
        ) -> ControlPlaneResult<()> {
            self.0.cancel_subagent_task(session_id, task_id).await
        }
        async fn set_plan_mode(
            &self,
            session_id: &str,
            params: serde_json::Value,
        ) -> ControlPlaneResult<()> {
            self.0.set_plan_mode(session_id, params).await
        }
        async fn inject_user_message(
            &self,
            session_id: &str,
            content: &str,
        ) -> ControlPlaneResult<()> {
            self.0.inject_user_message(session_id, content).await
        }
    }

    /// CP-02: `terminate` / `abort` used to acknowledge themselves. Without an adapter they
    /// must now report the missing capability instead of a silent success.
    #[tokio::test]
    async fn terminate_and_abort_are_not_silent_successes_without_an_adapter() {
        let bridge = ControlPlaneBridge::new(events_config());
        for outcome in [
            bridge.terminate("s1").await,
            bridge.abort("s1").await,
            bridge.compress("s1").await,
            bridge.plan_mode("s1", serde_json::json!({})).await,
            bridge.post_message("s1", serde_json::json!({ "content": "hi" })).await,
            bridge.cancel_subagent_task("s1", "t1").await,
        ] {
            assert!(
                matches!(outcome, Err(ControlPlaneError::Unsupported(_))),
                "expected Unsupported, got {outcome:?}"
            );
        }
        assert!(matches!(
            bridge.tasks("s1").await,
            Err(ControlPlaneError::Unsupported(_))
        ));
        assert!(matches!(
            bridge.subagent_tasks("s1").await,
            Err(ControlPlaneError::Unsupported(_))
        ));
    }

    /// CP-02: every command route reaches `adapter.execute_command` / the matching adapter
    /// hook, and `export-transcript` returns the history envelope.
    #[tokio::test]
    async fn contract_routes_delegate_to_the_attached_adapter() {
        let (bridge, adapter) = adapter_bridge();

        bridge.compress("s1").await.unwrap();
        bridge.terminate("s1").await.unwrap();
        bridge.abort("s1").await.unwrap();
        assert_eq!(
            *adapter.commands.lock().unwrap(),
            vec![
                COMMAND_COMPRESS.to_string(),
                COMMAND_TERMINATE.to_string(),
                COMMAND_ABORT.to_string()
            ]
        );

        let tasks = bridge.tasks("s1").await.unwrap();
        assert_eq!(tasks["tasks"][0]["id"], "t1");
        let subagent_tasks = bridge.subagent_tasks("s1").await.unwrap();
        assert_eq!(subagent_tasks["tasks"][0]["id"], "st1");
        bridge.cancel_subagent_task("s1", "st1").await.unwrap();
        assert_eq!(*adapter.cancelled.lock().unwrap(), vec!["st1".to_string()]);

        bridge
            .plan_mode("s1", serde_json::json!({ "active": true }))
            .await
            .unwrap();
        assert_eq!(
            adapter.plan_mode.lock().unwrap().as_ref().unwrap()["active"],
            true
        );

        bridge
            .post_message("s1", serde_json::json!({ "content": "hello" }))
            .await
            .unwrap();
        assert_eq!(*adapter.injected.lock().unwrap(), vec!["hello".to_string()]);

        let transcript = bridge.export_transcript("s1").await.unwrap();
        assert_eq!(transcript["sessionId"], "s1");
        assert_eq!(transcript["format"], "json");
        assert!(transcript["messages"].is_array());
    }

    /// CP-02: a busy session refuses `compress` / `messages` with 409 rather than racing
    /// the running turn (mirrors `SessionBridge.compress` / `postMessage`).
    #[tokio::test]
    async fn busy_sessions_are_rejected_on_write_routes() {
        let (bridge, adapter) = adapter_bridge();
        bridge.set_busy("s1", true);
        assert!(matches!(
            bridge.compress("s1").await,
            Err(ControlPlaneError::Busy(_))
        ));
        assert!(matches!(
            bridge.post_message("s1", serde_json::json!({ "content": "hi" }))
                .await,
            Err(ControlPlaneError::Busy(_))
        ));
        assert!(adapter.injected.lock().unwrap().is_empty());
        assert!(adapter.commands.lock().unwrap().is_empty());
    }

    /// CP-02: an empty `content` is a configuration error, not an empty injection.
    #[tokio::test]
    async fn post_message_requires_content() {
        let (bridge, adapter) = adapter_bridge();
        assert!(bridge
            .post_message("s1", serde_json::json!({ "content": "  " }))
            .await
            .is_err());
        assert!(adapter.injected.lock().unwrap().is_empty());
        // Java falls back to `message`, then to the raw body.
        bridge
            .post_message("s1", serde_json::json!({ "message": "from-message" }))
            .await
            .unwrap();
        assert_eq!(
            *adapter.injected.lock().unwrap(),
            vec!["from-message".to_string()]
        );
    }

    // ─── CP-04: Level-2 outbound loop ───

    /// Link double: records frames and never completes the downstream stream.
    #[derive(Default)]
    struct RecordingLink {
        sent: Mutex<Vec<String>>,
    }

    impl RecordingLink {
        fn event_reports(&self) -> Vec<EventReport> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .filter_map(|frame| {
                    let value: serde_json::Value = serde_json::from_str(frame).ok()?;
                    serde_json::from_value::<EventReport>(value.get("eventReport")?.clone()).ok()
                })
                .collect()
        }
    }

    #[async_trait]
    impl AsdpLink for RecordingLink {
        async fn send(&self, frame: &str) -> ControlPlaneResult<()> {
            self.sent.lock().unwrap().push(frame.to_string());
            Ok(())
        }
        async fn recv(&self) -> ControlPlaneResult<Option<String>> {
            futures::future::pending::<ControlPlaneResult<Option<String>>>().await
        }
        async fn close(&self) -> ControlPlaneResult<()> {
            Ok(())
        }
    }

    fn asdp_transport(link: Arc<dyn AsdpLink>) -> Arc<AsdpTransport> {
        Arc::new(AsdpTransport::new(
            link,
            UpstreamMeta::default(),
            ConnectRequest::default(),
        ))
    }

    fn commit(session_id: &str, seq: i32) -> SessionEventCursor {
        SessionEventCursor {
            session_id: session_id.to_string(),
            committed_seq: seq,
        }
    }

    /// CP-04: batching (20 per report), in-flight suppression and ack-driven commit.
    #[tokio::test]
    async fn flush_events_batches_the_outbox_and_acknowledges_it() {
        let mut bridge = outbox_bridge().await;
        let link = Arc::new(RecordingLink::default());
        let link_handle: Arc<dyn AsdpLink> = link.clone();
        let transport = asdp_transport(link_handle);
        transport.connect().await.unwrap();
        bridge.attach_asdp(Arc::clone(&transport));

        for i in 0..25 {
            bridge.record_event(
                SessionEvent::builder("s1", TYPE_MESSAGE)
                    .content(format!("m{i}"))
                    .build(),
            );
        }
        assert_eq!(bridge.pending_event_count(), 25);

        assert_eq!(bridge.flush_events().await.unwrap(), EVENT_BATCH_SIZE);
        let reports = link.event_reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].events.len(), EVENT_BATCH_SIZE);
        assert_eq!(reports[0].events[0].session_id, "s1");
        assert_eq!(reports[0].events[0].seq, 1);
        let first_id = reports[0].report_id.clone();

        // No ack yet: the report is in flight, so nothing else may leave.
        assert_eq!(bridge.flush_events().await.unwrap(), 0);
        assert_eq!(link.event_reports().len(), 1);

        bridge
            .on_event_ack(&EventReportAck {
                report_id: first_id,
                committed: vec![commit("s1", 20)],
                error: String::new(),
            })
            .unwrap();
        assert_eq!(bridge.pending_event_count(), 5);

        assert_eq!(bridge.flush_events().await.unwrap(), 5);
        let reports = link.event_reports();
        assert_eq!(reports.len(), 2);
        bridge
            .on_event_ack(&EventReportAck {
                report_id: reports[1].report_id.clone(),
                committed: vec![commit("s1", 25)],
                error: String::new(),
            })
            .unwrap();
        assert_eq!(bridge.pending_event_count(), 0);
    }

    /// CP-04: a report that was never acknowledged is resent with the **same** `report_id`
    /// after the 15s ack timeout (fake clock, no real wait).
    #[tokio::test]
    async fn in_flight_reports_are_resent_after_the_ack_timeout() {
        let now = Arc::new(AtomicU64::new(0));
        let base = Instant::now();
        let clock_now = Arc::clone(&now);
        let clock: Clock = Arc::new(move || {
            base + Duration::from_millis(clock_now.load(Ordering::SeqCst))
        });

        let mut bridge = outbox_bridge().await;
        bridge.set_clock(clock);
        let link = Arc::new(RecordingLink::default());
        let link_handle: Arc<dyn AsdpLink> = link.clone();
        let transport = asdp_transport(link_handle);
        transport.connect().await.unwrap();
        bridge.attach_asdp(Arc::clone(&transport));
        bridge.record_event(
            SessionEvent::builder("s1", TYPE_MESSAGE)
                .content("only")
                .build(),
        );

        assert_eq!(bridge.flush_events().await.unwrap(), 1);

        now.store(1_000, Ordering::SeqCst);
        assert_eq!(bridge.flush_events().await.unwrap(), 0);
        assert_eq!(link.event_reports().len(), 1);

        now.store(15_000, Ordering::SeqCst);
        assert_eq!(bridge.flush_events().await.unwrap(), 1);
        let reports = link.event_reports();
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].report_id, reports[1].report_id);
    }

    /// CP-04: a rejected report stays in the outbox so the pump can replay it.
    #[tokio::test]
    async fn rejected_reports_stay_in_the_outbox() {
        let mut bridge = outbox_bridge().await;
        let link = Arc::new(RecordingLink::default());
        let link_handle: Arc<dyn AsdpLink> = link.clone();
        let transport = asdp_transport(link_handle);
        transport.connect().await.unwrap();
        bridge.attach_asdp(Arc::clone(&transport));
        bridge.record_event(
            SessionEvent::builder("s1", TYPE_MESSAGE)
                .content("x")
                .build(),
        );
        bridge.flush_events().await.unwrap();
        let report_id = link.event_reports()[0].report_id.clone();
        assert!(bridge
            .on_event_ack(&EventReportAck {
                report_id,
                committed: vec![commit("s1", 1)],
                error: "duplicate seq".into(),
            })
            .is_err());
        assert_eq!(bridge.pending_event_count(), 1);
    }

    /// Link double that acknowledges every event report it receives.
    struct AutoAckLink {
        sent: Mutex<Vec<String>>,
        tx: tokio::sync::mpsc::UnboundedSender<String>,
        rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<String>>,
    }

    impl AutoAckLink {
        fn new() -> Arc<Self> {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                tx,
                rx: tokio::sync::Mutex::new(rx),
            })
        }
    }

    #[async_trait]
    impl AsdpLink for AutoAckLink {
        async fn send(&self, frame: &str) -> ControlPlaneResult<()> {
            self.sent.lock().unwrap().push(frame.to_string());
            let Ok(value) = serde_json::from_str::<serde_json::Value>(frame) else {
                return Ok(());
            };
            let Some(report) = value.get("eventReport") else {
                return Ok(());
            };
            let report_id = report
                .get("reportId")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            let mut committed: BTreeMap<String, i32> = BTreeMap::new();
            if let Some(events) = report.get("events").and_then(|value| value.as_array()) {
                for event in events {
                    let session_id = event
                        .get("sessionId")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string();
                    let seq = event.get("seq").and_then(|value| value.as_i64()).unwrap_or(0) as i32;
                    let entry = committed.entry(session_id).or_insert(0);
                    if seq > *entry {
                        *entry = seq;
                    }
                }
            }
            let ack = EventReportAck {
                report_id,
                committed: committed
                    .into_iter()
                    .map(|(session_id, seq)| commit(&session_id, seq))
                    .collect(),
                error: String::new(),
            };
            let _ = self.tx.send(serde_json::json!({ "eventAck": ack }).to_string());
            Ok(())
        }
        async fn recv(&self) -> ControlPlaneResult<Option<String>> {
            let mut rx = self.rx.lock().await;
            Ok(rx.recv().await)
        }
        async fn close(&self) -> ControlPlaneResult<()> {
            Ok(())
        }
    }

    /// CP-04: the 5s-interval pump plus the downstream reader drain the outbox on their own
    /// (the interval is shortened so the assertion needs no real 5s wait).
    #[tokio::test]
    async fn event_pump_flushes_and_applies_acks_until_the_outbox_is_empty() {
        let link = AutoAckLink::new();
        let link_handle: Arc<dyn AsdpLink> = link.clone();
        let transport = Arc::new(AsdpTransport::new(
            link_handle,
            UpstreamMeta::default(),
            ConnectRequest::default(),
        ));
        transport.connect().await.unwrap();

        let mut bridge = ControlPlaneBridge::new(events_config());
        bridge.attach_asdp(Arc::clone(&transport));
        for i in 0..3 {
            bridge.record_event(
                SessionEvent::builder("s1", TYPE_MESSAGE)
                    .content(format!("m{i}"))
                    .build(),
            );
        }
        bridge.spawn_event_pump(Arc::clone(&transport), Duration::from_millis(5));

        let mut drained = false;
        for _ in 0..200 {
            if bridge.pending_event_count() == 0 {
                drained = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            drained,
            "outbox still holds {} events",
            bridge.pending_event_count()
        );
        assert_eq!(EVENT_FLUSH_INTERVAL, Duration::from_millis(5_000));
        assert_eq!(EVENT_ACK_TIMEOUT, Duration::from_millis(15_000));
        bridge.close().unwrap();
    }
}
