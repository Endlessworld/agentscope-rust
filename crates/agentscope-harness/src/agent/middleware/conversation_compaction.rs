//! Conversation-compaction middleware, mirroring the Java `ConversationCompactionMiddleware`.
//!
//! Runs at the start of each agent call (`on_agent`): reads the accumulated
//! non-SYSTEM history from the shared agent state, asks the [`ConversationCompactor`]
//! whether it has grown past the trigger, and if so rewrites the state with the
//! compacted `[summary] + preserved_tail` replacement. Unlike a naive sliding window,
//! the preserved tail stays verbatim so recent context (and its prefix KV-cache
//! reuse) is untouched; only the summarized prefix is replaced.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{Msg, MsgRole};
use agentscope_core::middleware::{AgentInput, EventStream, Middleware, Next};
use futures::StreamExt;

use crate::agent::memory::compaction::{CompactionConfig, ConversationCompactor};

/// Middleware that compacts the accumulated conversation history before each
/// agent call when it exceeds the configured trigger.
pub struct ConversationCompactionMiddleware {
    compactor: Arc<ConversationCompactor>,
    config: CompactionConfig,
    agent_id: String,
}

impl ConversationCompactionMiddleware {
    /// Creates a middleware with the given compactor, config and agent id.
    pub fn new(
        compactor: Arc<ConversationCompactor>,
        config: CompactionConfig,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            compactor,
            config,
            agent_id: agent_id.into(),
        }
    }

    /// Convenience constructor using the agent's primary model for summarization.
    pub fn with_model(
        model: Arc<dyn agentscope_core::model::Model>,
        config: CompactionConfig,
        agent_id: impl Into<String>,
    ) -> Self {
        Self::new(Arc::new(ConversationCompactor::with_model(model)), config, agent_id)
    }
}

impl Middleware for ConversationCompactionMiddleware {
    fn on_agent(
        &self,
        ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        // No shared state handle (ad-hoc context) -> nothing to compact.
        let Some(state) = ctx.state.clone() else {
            return next(input);
        };
        let compactor = Arc::clone(&self.compactor);
        let config = self.config.clone();
        let agent_id = self.agent_id.clone();

        let fut = async move {
            let compacted = {
                let (history, session_id): (Vec<Msg>, String) = {
                    let guard = state.lock().unwrap();
                    let session_id = guard.session_id().to_string();
                    let history = guard
                        .context()
                        .iter()
                        .filter(|m| m.role != MsgRole::System)
                        .cloned()
                        .collect();
                    (history, session_id)
                };
                if history.is_empty() {
                    None
                } else {
                    match compactor
                        .compact_if_needed(history, &config, &agent_id, &session_id)
                        .await
                    {
                        Ok(replacement) => replacement,
                        Err(e) => {
                            tracing::warn!(
                                agent = %agent_id,
                                error = %e,
                                "conversation compaction failed; keeping full history"
                            );
                            None
                        }
                    }
                }
            };
            let summary = compacted.as_ref().and_then(|replacement| {
                let summary = replacement.first().map(|m| m.text_content());
                let mut guard = state.lock().unwrap();
                let sys: Vec<Msg> = guard
                    .context()
                    .iter()
                    .filter(|m| m.role == MsgRole::System)
                    .cloned()
                    .collect();
                let mut new_ctx = sys;
                new_ctx.extend(replacement.iter().cloned());
                *guard.context_mut() = new_ctx;
                tracing::debug!(agent = %agent_id, "conversation compacted");
                summary
            });
            // Announce the compaction in-band so the ACP layer can translate it
            // into a capability-gated `compaction_update` session notification.
            if let Some(summary) = summary {
                let event = compaction_event(&agent_id, &summary);
                return futures::stream::once(async move { Ok(event) })
                    .chain(next(input))
                    .boxed();
            }
            next(input)
        };
        Box::pin(futures::stream::once(fut).flatten())
    }
}

/// Builds the in-band marker event announcing a completed conversation
/// compaction.
///
/// The ACP layer intercepts this `Custom` event and translates it into a
/// capability-gated `compaction_update` session notification; other consumers
/// can ignore it.
fn compaction_event(agent_id: &str, summary: &str) -> agentscope_core::event::AgentEvent {
    let mut fields = serde_json::Map::new();
    fields.insert("kind".to_string(), serde_json::json!("compaction"));
    fields.insert("agent_id".to_string(), serde_json::json!(agent_id));
    fields.insert("summary".to_string(), serde_json::json!(summary));
    agentscope_core::event::AgentEvent::with_fields(
        agentscope_core::event::AgentEventType::Custom,
        fields,
    )
}