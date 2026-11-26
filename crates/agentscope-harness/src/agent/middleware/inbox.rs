//! Middleware that drains the message-bus inbox before each reasoning step and
//! injects the payloads as `HintBlock`s so the LLM can see them.
//!
//! Producers (e.g. `AsyncToolMiddleware`, team tools, schedulers) push hint
//! payloads into the per-session inbox via `MessageBus::inbox_push`. This
//! middleware drains them at the start of each reasoning step, appends them to
//! the reasoning input, and emits a `HintBlock` event for each so the
//! front-end event stream can render them in real time. It also surfaces stale
//! async tool records (RUNNING past the TTL) as hints that the tool may still be
//! running.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::event::{AgentEvent, AgentEventType};
use agentscope_core::message::{ContentBlock, HintBlock, JsonMap, Msg, MsgRole, TextBlock};
use agentscope_core::middleware::{EventStream, Middleware, Next, ReasoningInput};
use futures::StreamExt;
use serde_json::Value;

use crate::agent::bus::{AsyncToolRegistry, MessageBus};

/// Metadata key carrying the originating source of a background notification.
const METADATA_SOURCE: &str = "agentscope_source";

/// Metadata key carrying the producer-assigned hint id, for deduplication.
const METADATA_HINT_ID: &str = "agentscope_hint_id";

/// Renders a stale TTL for the notification text (whole minutes when possible).
fn humanize_stale_ttl(ttl: chrono::Duration) -> String {
    let minutes = ttl.num_minutes();
    if minutes >= 1 {
        format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
    } else {
        format!("{} seconds", ttl.num_seconds().max(0))
    }
}

/// Parses an inbox payload into a `HintBlock`; returns `None` when the payload
/// carries no `hint` field.
fn deserialize_hint_block(payload: &Value) -> Option<HintBlock> {
    let hint = payload.get("hint")?.as_str()?.to_string();
    Some(HintBlock {
        id: payload
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string),
        hint: Some(hint),
        source: payload
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Middleware that drains the per-session inbox and stale async tools into the
/// reasoning input before each model call.
pub struct InboxMiddleware {
    message_bus: Arc<dyn MessageBus>,
    max_drain_count: usize,
    async_tool_registry: Option<Arc<dyn AsyncToolRegistry>>,
    stale_ttl: chrono::Duration,
}

impl InboxMiddleware {
    /// Creates a middleware with the default drain cap (100) and stale TTL
    /// (10 minutes).
    pub fn new(message_bus: Arc<dyn MessageBus>) -> Self {
        Self::builder(message_bus).build()
    }

    /// Returns a builder for a more configurable instance.
    pub fn builder(message_bus: Arc<dyn MessageBus>) -> InboxMiddlewareBuilder {
        InboxMiddlewareBuilder::new(message_bus)
    }
}

/// Builder for [`InboxMiddleware`].
pub struct InboxMiddlewareBuilder {
    message_bus: Arc<dyn MessageBus>,
    max_drain_count: usize,
    async_tool_registry: Option<Arc<dyn AsyncToolRegistry>>,
    stale_ttl: chrono::Duration,
}

impl InboxMiddlewareBuilder {
    /// Creates a builder with defaults (drain 100, stale TTL 10 min).
    pub fn new(message_bus: Arc<dyn MessageBus>) -> Self {
        Self {
            message_bus,
            max_drain_count: 100,
            async_tool_registry: None,
            stale_ttl: chrono::Duration::minutes(10),
        }
    }

    /// Sets the maximum number of inbox entries drained per reasoning step.
    pub fn max_drain_count(mut self, max_drain_count: usize) -> Self {
        self.max_drain_count = max_drain_count;
        self
    }

    /// Enables stale async-tool surfacing with the given registry.
    pub fn async_tool_registry(mut self, registry: Arc<dyn AsyncToolRegistry>) -> Self {
        self.async_tool_registry = Some(registry);
        self
    }

    /// Sets the stale TTL for async tool records.
    pub fn stale_ttl(mut self, stale_ttl: chrono::Duration) -> Self {
        self.stale_ttl = stale_ttl;
        self
    }

    /// Builds the middleware.
    pub fn build(self) -> InboxMiddleware {
        InboxMiddleware {
            message_bus: self.message_bus,
            max_drain_count: self.max_drain_count,
            async_tool_registry: self.async_tool_registry,
            stale_ttl: self.stale_ttl,
        }
    }
}

impl Middleware for InboxMiddleware {
    fn on_reasoning(
        &self,
        ctx: &RuntimeContext,
        input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> EventStream {
        let Some(session_id) = ctx.session_id().map(str::to_string) else {
            return next(input);
        };

        let message_bus = Arc::clone(&self.message_bus);
        let async_registry = self.async_tool_registry.clone();
        let max_drain_count = self.max_drain_count;
        let stale_ttl = self.stale_ttl;
        let agent_name = ctx.agent_name.clone().unwrap_or_default();
        let state = ctx.state.clone();

        let fut = async move {
            let mut input = input;
            // Drain the per-session inbox. Draining is destructive per entry, so a failed call
            // must not be silently swallowed: report it and continue without those hints —
            // requeueing would risk double delivery on the next step.
            let entries = match message_bus.inbox_drain(&session_id, max_drain_count).await {
                Ok(entries) => entries,
                Err(e) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %e,
                        "inbox drain failed; its background notifications are skipped this step"
                    );
                    Vec::new()
                }
            };
            // `persist: false` marks a one-shot notification whose durable counterpart is
            // delivered through another channel (the subagent task result): injecting it into
            // `state.context` would replay the same completion note on every later request.
            let mut hints: Vec<(HintBlock, bool)> = Vec::new();
            for entry in entries {
                if let Some(h) = deserialize_hint_block(&entry.payload) {
                    let persist = entry
                        .payload
                        .get("persist")
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                    hints.push((h, persist));
                }
            }

            // Surface stale async tool records as "still running" hints.
            if let Some(registry) = &async_registry {
                match registry.find_stale(&session_id, stale_ttl).await {
                    Ok(stale) => {
                        for rec in stale {
                            // A failed status flip must not silence the notification; the stable
                            // hint id below keeps it deduplicated across steps instead.
                            if let Err(e) = registry.mark_timeout(&rec.id).await {
                                tracing::warn!(
                                    record_id = %rec.id,
                                    error = %e,
                                    "failed to mark a stale async tool record as timed out"
                                );
                            }
                            hints.push((
                                HintBlock::builder()
                                    // Stable per record: the context deduplicates by hint id, so
                                    // a random id re-announced the same record on every step
                                    // whose timeout had not been persisted.
                                    .id(format!("stale:{}", rec.id))
                                    .hint(format!(
                                        "<system-notification>Tool '{}' (id={}) has not completed \
                                         within {}; it may still be running — call wait_async to \
                                         keep waiting, or retry if the result is no longer \
                                         needed.</system-notification>",
                                        rec.tool_name,
                                        rec.tool_call_id,
                                        humanize_stale_ttl(stale_ttl)
                                    ))
                                    .source("system")
                                    .build(),
                                true,
                            ));
                        }
                    }
                    Err(e) => tracing::warn!(
                        session_id = %session_id,
                        error = %e,
                        "failed to scan for stale async tool records"
                    ),
                }
            }

            // Without agent state the notifications can only reach this step's messages: the
            // dedup set below and the durable copy in `state.context` both need the state.
            if state.is_none() {
                tracing::warn!(
                    session_id = %session_id,
                    "runtime context has no agent state; background notifications are visible in \
                     this step only and will not be replayed on later steps"
                );
            }

            // A hint that already reached the context must not be injected again.
            //
            // The notification is written to `state.context` as well as to this step's messages
            // (see below), and `state.context` is replayed on every model call, so any re-entry —
            // a resumed run, a re-executed step, a refreshed session — used to re-announce the
            // same background completion as if it had just arrived.
            if let Some(state) = state.as_ref() {
                let delivered: std::collections::HashSet<String> = {
                    // Scoped so the std::sync::Mutex guard is never held across an await.
                    let guard = match state.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    guard
                        .context
                        .iter()
                        .filter_map(|message| {
                            message
                                .metadata()
                                .get(METADATA_HINT_ID)
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .collect()
                };
                hints.retain(|(hint, _)| {
                    hint.id
                        .as_ref()
                        .map(|id| !delivered.contains(id))
                        .unwrap_or(true)
                });
            }

            if hints.is_empty() {
                return next(input);
            }

            // Emit a HintBlock event for each hint.
            let mut events: Vec<agentscope_core::error::Result<AgentEvent>> =
                Vec::with_capacity(hints.len());
            for (h, _) in &hints {
                let mut fields = JsonMap::new();
                if let Some(id) = &h.id {
                    fields.insert("id".to_string(), Value::String(id.clone()));
                }
                if let Some(text) = &h.hint {
                    fields.insert("hint".to_string(), Value::String(text.clone()));
                }
                if let Some(src) = &h.source {
                    fields.insert("source".to_string(), Value::String(src.clone()));
                }
                events.push(Ok(AgentEvent::with_fields(
                    AgentEventType::HintBlock,
                    fields,
                )));
            }

            // Each hint becomes its own user-role notification carrying a provenance
            // sentence and structured metadata, mirroring Java `injectHintsToContext`.
            // Batching them into one assistant message (the previous behaviour) made the
            // model read background events as its own prior output.
            let notifications: Vec<(Msg, bool)> = hints
                .iter()
                .map(|(h, persist)| {
                    let source = h
                        .source
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("background");
                    let mut blocks = vec![ContentBlock::Text(TextBlock::new(format!(
                        "Automated background event from {source}. This is context, not a \
                         user message or approval."
                    )))];
                    if let Some(text) = &h.hint {
                        blocks.push(ContentBlock::Text(TextBlock::new(text.clone())));
                    }
                    let mut metadata = JsonMap::new();
                    metadata.insert(Msg::METADATA_SYNTHETIC.to_string(), Value::Bool(true));
                    metadata.insert(
                        Msg::METADATA_REMINDER_KIND.to_string(),
                        Value::String("background_event".to_string()),
                    );
                    metadata.insert(
                        METADATA_SOURCE.to_string(),
                        Value::String(source.to_string()),
                    );
                    if let Some(id) = &h.id {
                        metadata.insert(METADATA_HINT_ID.to_string(), Value::String(id.clone()));
                    }
                    let message = Msg::builder()
                        .role(MsgRole::User)
                        .name(agent_name.clone())
                        .content(blocks)
                        .metadata(metadata)
                        .build();
                    (message, *persist)
                })
                .collect();

            // Persistent hints also reach the accumulated context so they survive into later
            // turns; Java writes them to `AgentState.context`. One-shot notifications stay in
            // this request only: their result arrives through the delivery channel.
            let persistent: Vec<Msg> = notifications
                .iter()
                .filter(|(_, persist)| *persist)
                .map(|(message, _)| message.clone())
                .collect();
            if let Some(state) = state {
                // Scoped so the std::sync::Mutex guard is never held across an await.
                let mut guard = match state.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.context_mut().extend(persistent);
            }
            input
                .messages
                .extend(notifications.into_iter().map(|(message, _)| message));

            let chain = futures::stream::iter(events).chain(next(input));
            Box::pin(chain)
        };

        Box::pin(futures::stream::once(fut).flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::MsgRole;

    fn notification_for(source: Option<&str>, hint: &str, id: Option<&str>) -> Msg {
        // Mirrors the shape produced by `on_reasoning`: one user-role notification per
        // hint, with a provenance sentence plus structured metadata.
        let source_label = source
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("background");
        let mut blocks = vec![ContentBlock::Text(TextBlock::new(format!(
            "Automated background event from {source_label}. This is context, not a user \
             message or approval."
        )))];
        blocks.push(ContentBlock::Text(TextBlock::new(hint.to_string())));
        let mut metadata = JsonMap::new();
        metadata.insert(Msg::METADATA_SYNTHETIC.to_string(), Value::Bool(true));
        metadata.insert(
            Msg::METADATA_REMINDER_KIND.to_string(),
            Value::String("background_event".to_string()),
        );
        metadata.insert(
            METADATA_SOURCE.to_string(),
            Value::String(source_label.to_string()),
        );
        if let Some(id) = id {
            metadata.insert(METADATA_HINT_ID.to_string(), Value::String(id.to_string()));
        }
        Msg::builder()
            .role(MsgRole::User)
            .content(blocks)
            .metadata(metadata)
            .build()
    }

    #[test]
    fn notifications_are_user_role_and_carry_provenance() {
        let msg = notification_for(Some("system"), "do the thing", Some("h1"));
        assert_eq!(msg.role(), MsgRole::User);
        let text = msg.text_content();
        assert!(
            text.contains("Automated background event from system"),
            "{text}"
        );
        assert!(text.contains("do the thing"), "{text}");
        assert_eq!(
            msg.metadata().get(Msg::METADATA_SYNTHETIC),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            msg.metadata()
                .get(Msg::METADATA_REMINDER_KIND)
                .and_then(Value::as_str),
            Some("background_event")
        );
        assert_eq!(
            msg.metadata().get(METADATA_SOURCE).and_then(Value::as_str),
            Some("system")
        );
        assert_eq!(
            msg.metadata().get(METADATA_HINT_ID).and_then(Value::as_str),
            Some("h1")
        );
    }

    #[test]
    fn a_missing_source_falls_back_to_background() {
        let msg = notification_for(None, "anything", None);
        assert!(msg
            .text_content()
            .contains("Automated background event from background"));
        assert_eq!(
            msg.metadata().get(METADATA_SOURCE).and_then(Value::as_str),
            Some("background")
        );
        assert!(msg.metadata().get(METADATA_HINT_ID).is_none());
    }

    // ---- middleware behaviour with a real bus / registry ----

    use crate::agent::bus::{BusEntry, WorkspaceAsyncToolRegistry, WorkspaceMessageBus};
    use crate::filesystem::{Filesystem, LocalFilesystem};
    use crate::AsyncToolRecord;
    use agentscope_core::model::GenerateOptions;
    use agentscope_core::state::AgentState;

    async fn temp_dir(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("asr_inbox_{tag}_{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&base).await;
        tokio::fs::create_dir_all(&base).await.unwrap();
        base
    }

    /// Runs the middleware once, returning the emitted events, the messages handed downstream
    /// and whether `next` was invoked.
    async fn run(
        mw: &InboxMiddleware,
        ctx: &RuntimeContext,
    ) -> (
        Vec<agentscope_core::error::Result<AgentEvent>>,
        Vec<Msg>,
        bool,
    ) {
        let captured: Arc<std::sync::Mutex<Vec<Msg>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let invoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sink = Arc::clone(&captured);
        let flag = Arc::clone(&invoked);
        let next: Next<ReasoningInput> = Box::new(move |input: ReasoningInput| {
            *sink.lock().unwrap() = input.messages.clone();
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            let stream: EventStream = Box::pin(futures::stream::empty());
            stream
        });
        let input = ReasoningInput::new(vec![], vec![], GenerateOptions::default());
        let events = mw.on_reasoning(ctx, input, next).collect::<Vec<_>>().await;
        let msgs = captured.lock().unwrap().clone();
        (
            events,
            msgs,
            invoked.load(std::sync::atomic::Ordering::SeqCst),
        )
    }

    async fn stale_registry(
        dir: &std::path::Path,
    ) -> (Arc<dyn MessageBus>, Arc<WorkspaceAsyncToolRegistry>) {
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(dir));
        let bus: Arc<dyn MessageBus> = Arc::new(WorkspaceMessageBus::new(fs.clone(), "/bus"));
        let registry = Arc::new(WorkspaceAsyncToolRegistry::new(fs, "/async-tools"));
        let old = chrono::Utc::now() - chrono::Duration::hours(1);
        registry
            .register(AsyncToolRecord::new(
                "rec-1", "sess-1", "execute", "call-1", old,
            ))
            .await
            .expect("register");
        (bus, registry)
    }

    fn hint_events(events: &[agentscope_core::error::Result<AgentEvent>]) -> usize {
        let mut count = 0;
        for event in events {
            if let Ok(event) = event {
                if event.event_type() == AgentEventType::HintBlock {
                    count += 1;
                }
            }
        }
        count
    }

    #[tokio::test]
    async fn a_stale_async_tool_is_reported_as_still_running() {
        let dir = temp_dir("stale").await;
        let (bus, registry) = stale_registry(&dir).await;
        let mw = InboxMiddleware::builder(bus)
            .async_tool_registry(registry.clone())
            .build();
        let ctx = RuntimeContext::default().with_session_id("sess-1");

        let (events, msgs, _) = run(&mw, &ctx).await;
        assert_eq!(hint_events(&events), 1, "expected one hint: {events:?}");
        assert_eq!(msgs.len(), 1);
        let text = msgs[0].text_content();
        assert!(text.contains("may still be running"), "{text}");
        assert!(text.contains("wait_async"), "{text}");
        assert!(text.contains("10 minutes"), "{text}");
        // The record carries no heartbeat, so a crash must not be asserted as fact.
        assert!(!text.contains("process crash"), "{text}");
        assert_eq!(
            msgs[0]
                .metadata()
                .get(METADATA_HINT_ID)
                .and_then(Value::as_str),
            Some("stale:rec-1")
        );

        // The successful status flip keeps the record from being reported again.
        let records = registry.list_session("sess-1").await.expect("records");
        assert_eq!(records[0].status, AsyncToolRecord::TIMEOUT);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn one_shot_hints_are_injected_once_without_entering_the_context() {
        // Subagent completion hints are pointers: the actual result is delivered by
        // `SubagentsMiddleware`. Persisting the pointer made every completion leave a
        // "delivered next step" note in the context that was replayed forever.
        let dir = temp_dir("transient_hint").await;
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
        let bus: Arc<dyn MessageBus> = Arc::new(WorkspaceMessageBus::new(fs, "/bus"));
        bus.inbox_push(
            "sess-1",
            serde_json::json!({
                "type": "hint",
                "id": "one-shot-1",
                "hint": "task completed",
                "source": "subagent_task",
                "persist": false,
            }),
        )
        .await
        .expect("push");
        let state = Arc::new(std::sync::Mutex::new(AgentState::builder().build()));
        let mut ctx = RuntimeContext::default().with_session_id("sess-1");
        ctx.state = Some(Arc::clone(&state));

        let (events, msgs, invoked) = run(&InboxMiddleware::new(bus), &ctx).await;
        assert!(invoked);
        assert_eq!(hint_events(&events), 1);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].text_content().contains("task completed"));
        assert!(
            state.lock().unwrap().context().is_empty(),
            "a one-shot hint must not be replayed on later steps"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn default_hints_still_enter_the_context() {
        let dir = temp_dir("persistent_hint").await;
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
        let bus: Arc<dyn MessageBus> = Arc::new(WorkspaceMessageBus::new(fs, "/bus"));
        bus.inbox_push(
            "sess-1",
            serde_json::json!({
                "type": "hint",
                "id": "persistent-1",
                "hint": "background tool result",
                "source": "tool_output",
            }),
        )
        .await
        .expect("push");
        let state = Arc::new(std::sync::Mutex::new(AgentState::builder().build()));
        let mut ctx = RuntimeContext::default().with_session_id("sess-1");
        ctx.state = Some(Arc::clone(&state));

        let (events, msgs, invoked) = run(&InboxMiddleware::new(bus), &ctx).await;
        assert!(invoked);
        assert_eq!(hint_events(&events), 1);
        assert_eq!(msgs.len(), 1);
        let seen = state.lock().unwrap();
        assert_eq!(seen.context().len(), 1);
        assert_eq!(
            seen.context()[0].text_content(),
            msgs[0].text_content(),
            "a persistent hint must be replayed verbatim"
        );
        drop(seen);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_stale_hint_already_in_the_context_is_not_injected_again() {
        let dir = temp_dir("stale_dedupe").await;
        let (bus, registry) = stale_registry(&dir).await;
        let mw = InboxMiddleware::builder(bus)
            .async_tool_registry(registry)
            .build();

        // The same record was announced on an earlier step; the stable hint id is what makes
        // the deduplication possible (a random id re-announced it every step).
        let state = Arc::new(std::sync::Mutex::new(AgentState::builder().build()));
        state.lock().unwrap().context_mut().push(notification_for(
            Some("system"),
            "stale hint",
            Some("stale:rec-1"),
        ));
        let mut ctx = RuntimeContext::default().with_session_id("sess-1");
        ctx.state = Some(state);

        let (events, msgs, invoked) = run(&mw, &ctx).await;
        assert!(invoked);
        assert!(msgs.is_empty(), "the hint must not be injected twice");
        assert_eq!(hint_events(&events), 0);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// A bus whose drain always fails: the step must still run, without hints.
    struct FailingDrainBus {
        inner: Arc<WorkspaceMessageBus>,
    }

    #[async_trait::async_trait]
    impl MessageBus for FailingDrainBus {
        async fn queue_push(
            &self,
            key: &str,
            payload: Value,
        ) -> agentscope_core::error::Result<String> {
            self.inner.queue_push(key, payload).await
        }

        async fn queue_drain(
            &self,
            _key: &str,
            _max_count: usize,
        ) -> agentscope_core::error::Result<Vec<BusEntry>> {
            Err(agentscope_core::error::Error::Other(
                "drain exploded".to_string(),
            ))
        }

        async fn queue_delete(&self, key: &str) -> agentscope_core::error::Result<()> {
            self.inner.queue_delete(key).await
        }

        async fn queue_peek(&self, key: &str) -> agentscope_core::error::Result<bool> {
            self.inner.queue_peek(key).await
        }

        async fn log_append(
            &self,
            key: &str,
            payload: Value,
            max_len: i32,
        ) -> agentscope_core::error::Result<String> {
            self.inner.log_append(key, payload, max_len).await
        }

        async fn log_read(
            &self,
            key: &str,
            since: Option<&str>,
            max_count: usize,
        ) -> agentscope_core::error::Result<Vec<BusEntry>> {
            self.inner.log_read(key, since, max_count).await
        }

        async fn log_trim(&self, key: &str) -> agentscope_core::error::Result<()> {
            self.inner.log_trim(key).await
        }

        async fn publish(&self, key: &str, payload: Value) -> agentscope_core::error::Result<()> {
            self.inner.publish(key, payload).await
        }

        fn subscribe(&self, key: &str) -> tokio::sync::mpsc::UnboundedReceiver<Value> {
            self.inner.subscribe(key)
        }
    }

    #[tokio::test]
    async fn a_failed_drain_does_not_abort_the_step() {
        let dir = temp_dir("drain_fail").await;
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
        let inner = Arc::new(WorkspaceMessageBus::new(fs, "/bus"));
        let bus: Arc<dyn MessageBus> = Arc::new(FailingDrainBus { inner });
        let mw = InboxMiddleware::new(bus);
        let ctx = RuntimeContext::default().with_session_id("sess-1");

        let (events, msgs, invoked) = run(&mw, &ctx).await;
        assert!(invoked, "the step must run despite the drain failure");
        assert!(events.is_empty() && msgs.is_empty());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
