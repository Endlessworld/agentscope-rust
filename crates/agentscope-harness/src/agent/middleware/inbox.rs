//! Middleware that drains the message-bus inbox before each reasoning step and
//! injects the payloads as `HintBlock`s so the LLM can see them.
//!
//! Producers (e.g. `AsyncToolMiddleware`, team tools, schedulers) push hint
//! payloads into the per-session inbox via `MessageBus::inbox_push`. This
//! middleware drains them at the start of each reasoning step, appends them to
//! the reasoning input, and emits a `HintBlock` event for each so the
//! front-end event stream can render them in real time. It also surfaces stale
//! async tool records (crashed/timeout) as timeout hints.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::event::{AgentEvent, AgentEventType};
use agentscope_core::message::{ContentBlock, HintBlock, JsonMap, Msg, MsgRole, TextBlock};
use agentscope_core::middleware::{EventStream, Middleware, Next, ReasoningInput};
use futures::StreamExt;
use serde_json::Value;

use crate::agent::bus::{AsyncToolRegistry, MessageBus};

/// Generates a UUID without dashes, matching the Java side.
fn new_hex() -> String {
    uuid::Uuid::new_v4().to_string().replace('-', "")
}

/// Metadata key carrying the originating source of a background notification.
const METADATA_SOURCE: &str = "agentscope_source";

/// Metadata key carrying the producer-assigned hint id, for deduplication.
const METADATA_HINT_ID: &str = "agentscope_hint_id";

/// Parses an inbox payload into a `HintBlock`; returns `None` when the payload
/// carries no `hint` field.
fn deserialize_hint_block(payload: &Value) -> Option<HintBlock> {
    let hint = payload.get("hint")?.as_str()?.to_string();
    Some(HintBlock {
        id: payload.get("id").and_then(Value::as_str).map(str::to_string),
        hint: Some(hint),
        source: payload.get("source").and_then(Value::as_str).map(str::to_string),
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
            // Drain the per-session inbox.
            let entries = message_bus
                .inbox_drain(&session_id, max_drain_count)
                .await
                .unwrap_or_default();
            let mut hints: Vec<HintBlock> = Vec::new();
            for entry in entries {
                if let Some(h) = deserialize_hint_block(&entry.payload) {
                    hints.push(h);
                }
            }

            // Surface stale async tool records as timeout hints.
            if let Some(registry) = &async_registry {
                if let Ok(stale) = registry.find_stale(&session_id, stale_ttl).await {
                    for rec in stale {
                        let _ = registry.mark_timeout(&rec.id).await;
                        hints.push(
                            HintBlock::builder()
                                .id(new_hex())
                                .hint(format!(
                                    "<system-notification>Tool '{}' (id={}) did not complete                                      (process crash or timeout). You may retry or proceed                                      without the result.</system-notification>",
                                    rec.tool_name, rec.tool_call_id
                                ))
                                .source("system")
                                .build(),
                        );
                    }
                }
            }

            if hints.is_empty() {
                return next(input);
            }

            // Emit a HintBlock event for each hint.
            let mut events: Vec<agentscope_core::error::Result<AgentEvent>> =
                Vec::with_capacity(hints.len());
            for h in &hints {
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
                events.push(Ok(AgentEvent::with_fields(AgentEventType::HintBlock, fields)));
            }

            // Each hint becomes its own user-role notification carrying a provenance
            // sentence and structured metadata, mirroring Java `injectHintsToContext`.
            // Batching them into one assistant message (the previous behaviour) made the
            // model read background events as its own prior output.
            let notifications: Vec<Msg> = hints
                .iter()
                .map(|h| {
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
                    metadata.insert(
                        Msg::METADATA_SYNTHETIC.to_string(),
                        Value::Bool(true),
                    );
                    metadata.insert(
                        Msg::METADATA_REMINDER_KIND.to_string(),
                        Value::String("background_event".to_string()),
                    );
                    metadata.insert(
                        METADATA_SOURCE.to_string(),
                        Value::String(source.to_string()),
                    );
                    if let Some(id) = &h.id {
                        metadata.insert(
                            METADATA_HINT_ID.to_string(),
                            Value::String(id.clone()),
                        );
                    }
                    Msg::builder()
                        .role(MsgRole::User)
                        .name(agent_name.clone())
                        .content(blocks)
                        .metadata(metadata)
                        .build()
                })
                .collect();

            // The hints must also reach the accumulated context so they survive into
            // later turns; Java writes them to `AgentState.context`.
            if let Some(state) = state {
                // Scoped so the std::sync::Mutex guard is never held across an await.
                let mut guard = match state.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.context.extend(notifications.iter().cloned());
            }
            input.messages.extend(notifications);

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
        assert!(text.contains("Automated background event from system"), "{text}");
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
}