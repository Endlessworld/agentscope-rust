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
use agentscope_core::message::{ContentBlock, HintBlock, JsonMap, Msg, MsgRole};
use agentscope_core::middleware::{EventStream, Middleware, Next, ReasoningInput};
use futures::StreamExt;
use serde_json::Value;

use crate::agent::bus::{AsyncToolRegistry, MessageBus};

/// Generates a UUID without dashes, matching the Java side.
fn new_hex() -> String {
    uuid::Uuid::new_v4().to_string().replace('-', "")
}

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

            // Append a single assistant message carrying all hints to the input.
            let hint_blocks: Vec<ContentBlock> =
                hints.into_iter().map(ContentBlock::Hint).collect();
            input.messages.push(
                Msg::builder()
                    .role(MsgRole::Assistant)
                    .name(agent_name)
                    .content(hint_blocks)
                    .build(),
            );

            let chain = futures::stream::iter(events).chain(next(input));
            Box::pin(chain)
        };

        Box::pin(futures::stream::once(fut).flatten())
    }
}
