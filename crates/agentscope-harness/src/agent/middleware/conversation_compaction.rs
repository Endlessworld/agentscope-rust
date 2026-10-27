//! Conversation-compaction middleware, mirroring the Java `ConversationCompactionMiddleware`.
//!
//! Runs at the start of each agent call (`on_agent`): reads the accumulated
//! non-SYSTEM history from the shared agent state, asks the [`ConversationCompactor`]
//! whether it has grown past the trigger, and if so rewrites the state with the
//! compacted `[summary] + preserved_tail` replacement. Unlike a naive sliding window,
//! the preserved tail stays verbatim so recent context (and its prefix KV-cache
//! reuse) is untouched; only the summarized prefix is replaced.

use std::sync::{Arc, Mutex};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{Msg, MsgRole};
use agentscope_core::middleware::{
    AgentInput, EventStream, ExtensionPoint, Middleware, Next, ReasoningInput,
};
use agentscope_core::session::{SessionRecorder, RECORDER_CONTEXT_KEY};
use agentscope_core::state::AgentState;
use futures::StreamExt;

use crate::agent::memory::compaction::{
    CompactionConfig, CompactionMetrics, ConversationCompactor,
};

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
        Self::new(
            Arc::new(ConversationCompactor::with_model(model)),
            config,
            agent_id,
        )
    }
}

/// One completed compaction.
struct CompactedConversation {
    /// `[summary] + preserved_tail`.
    replacement: Vec<Msg>,
    /// Summary text announced to the client.
    summary: String,
    /// Quantified outcome for the durable and in-band facts.
    metrics: CompactionMetrics,
}

/// Reads, compacts and rewrites the shared conversation history.
///
/// `history_override` lets the per-iteration path compact exactly the history that is
/// about to be sent (after earlier `on_reasoning` rewrites) instead of re-reading the
/// unmodified state. Returns `None` when no compaction was needed or it failed.
async fn compact_conversation(
    state: Arc<Mutex<AgentState>>,
    recorder: Option<Arc<SessionRecorder>>,
    compactor: Arc<ConversationCompactor>,
    config: CompactionConfig,
    agent_id: String,
    history_override: Option<Vec<Msg>>,
    observed_tokens: Option<usize>,
) -> Option<CompactedConversation> {
    let (history, system, session_id) = {
        let guard = state.lock().unwrap();
        let history = match history_override {
            Some(history) => history,
            None => guard
                .context()
                .iter()
                .filter(|message| message.role != MsgRole::System)
                .cloned()
                .collect(),
        };
        let system: Vec<Msg> = guard
            .context()
            .iter()
            .filter(|message| message.role == MsgRole::System)
            .cloned()
            .collect();
        (history, system, guard.session_id().to_string())
    };
    if history.is_empty() {
        return None;
    }

    let (replacement, metrics) = match compactor
        .compact_if_needed_with_metrics_observed(
            history,
            &config,
            &agent_id,
            &session_id,
            recorder.as_ref(),
            observed_tokens,
        )
        .await
    {
        Ok((Some(replacement), metrics)) => (replacement, metrics),
        Ok((None, _)) => return None,
        Err(error) => {
            tracing::warn!(
                agent = %agent_id,
                error = %error,
                "conversation compaction failed; keeping full history"
            );
            return None;
        }
    };

    // The retained tail's usage records were produced against the old (pre-summary)
    // prefix. Anchoring on them would count a request the provider never saw.
    let replacement = crate::agent::context::strip_provider_usage(replacement);
    let summary = replacement
        .first()
        .map(|message| message.text_content())
        .unwrap_or_default();
    {
        let mut guard = state.lock().unwrap();
        let mut new_context = system;
        new_context.extend(replacement.iter().cloned());
        *guard.context_mut() = new_context;
    }
    tracing::debug!(agent = %agent_id, "conversation compacted");
    Some(CompactedConversation {
        replacement,
        summary,
        metrics,
    })
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
        let model_name = self.compactor.model_name().map(str::to_string);
        // A recorder attached by the session scheduler makes `compaction/start` and
        // `compaction/end` durable session facts instead of only an in-band marker.
        let recorder = ctx.extension::<SessionRecorder>(RECORDER_CONTEXT_KEY);

        let fut = async move {
            // The provider's latest `input_tokens` is the authority; only the messages
            // appended after that reply are locally estimated.
            let (history, observed_tokens) = {
                let guard = state.lock().unwrap();
                let history: Vec<Msg> = guard
                    .context()
                    .iter()
                    .filter(|message| message.role != MsgRole::System)
                    .cloned()
                    .collect();
                let usage =
                    crate::agent::context::context_usage(&history, &[], model_name.as_deref());
                (history, (usage.tokens > 0).then_some(usage.tokens as usize))
            };
            match compact_conversation(
                state,
                recorder,
                compactor,
                config,
                agent_id.clone(),
                Some(history),
                observed_tokens,
            )
            .await
            {
                Some(compacted) => {
                    // Announce the compaction in-band so the ACP layer can translate it
                    // into a capability-gated `compaction_update` session notification.
                    let event =
                        compaction_event(&agent_id, &compacted.summary, Some(&compacted.metrics));
                    futures::stream::once(async move { Ok(event) })
                        .chain(next(input))
                        .boxed()
                }
                None => next(input),
            }
        };
        Box::pin(futures::stream::once(fut).flatten())
    }

    /// Compacts before every reasoning step, not only once per turn.
    ///
    /// A long autonomous turn (ACP runs `max_iters = 9999`) can cross the model budget
    /// between two `on_agent` hooks. This hook keeps the threshold check on the real
    /// per-step history and rewrites the same input the model is about to receive, so
    /// compaction happens *before* a request is compiled instead of after it overflows.
    fn on_reasoning(
        &self,
        ctx: &RuntimeContext,
        input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> EventStream {
        let Some(state) = ctx.state.clone() else {
            return next(input);
        };
        let compactor = Arc::clone(&self.compactor);
        let config = self.config.clone();
        let agent_id = self.agent_id.clone();
        let model_name = self.compactor.model_name().map(str::to_string);
        let recorder = ctx.extension::<SessionRecorder>(RECORDER_CONTEXT_KEY);
        // `on_reasoning` events are not forwarded by the core stream; publish the marker
        // through the delta channel so ACP still receives `compaction_update`.
        let delta_tx = ctx.delta_tx.clone();

        let fut = async move {
            let system: Vec<Msg> = input
                .messages
                .iter()
                .filter(|message| message.role == MsgRole::System)
                .cloned()
                .collect();
            let history: Vec<Msg> = input
                .messages
                .iter()
                .filter(|message| message.role != MsgRole::System)
                .cloned()
                .collect();
            // This is the exact shape about to be sent, so the provider-anchored count
            // also covers the current system prompt and tool schemas.
            let usage = crate::agent::context::context_usage(
                &input.messages,
                &input.tools,
                model_name.as_deref(),
            );
            let observed_tokens = (usage.tokens > 0).then_some(usage.tokens as usize);
            match compact_conversation(
                state,
                recorder,
                compactor,
                config,
                agent_id.clone(),
                Some(history),
                observed_tokens,
            )
            .await
            {
                Some(compacted) => {
                    if let Some(tx) = &delta_tx {
                        let _ = tx.unbounded_send(Ok(compaction_event(
                            &agent_id,
                            &compacted.summary,
                            Some(&compacted.metrics),
                        )));
                    }
                    let mut messages = system;
                    messages.extend(compacted.replacement);
                    next(ReasoningInput::new(messages, input.tools, input.options))
                }
                None => next(input),
            }
        };
        Box::pin(futures::stream::once(fut).flatten())
    }

    fn active_points(&self) -> Vec<ExtensionPoint> {
        vec![ExtensionPoint::OnAgent, ExtensionPoint::OnReasoning]
    }
}

/// Builds the in-band marker event announcing a completed conversation
/// compaction.
///
/// The ACP layer intercepts this `Custom` event and translates it into a
/// capability-gated `compaction_update` session notification; other consumers
/// can ignore it. The quantified counts are included when available so an
/// in-band observer sees the same numbers as the durable `compaction/end` fact.
pub(crate) fn compaction_event(
    agent_id: &str,
    summary: &str,
    metrics: Option<&crate::agent::memory::compaction::CompactionMetrics>,
) -> agentscope_core::event::AgentEvent {
    let mut fields = serde_json::Map::new();
    fields.insert("kind".to_string(), serde_json::json!("compaction"));
    fields.insert("agent_id".to_string(), serde_json::json!(agent_id));
    fields.insert("summary".to_string(), serde_json::json!(summary));
    if let Some(metrics) = metrics {
        fields.insert(
            "compaction_id".to_string(),
            serde_json::json!(metrics.compaction_id),
        );
        fields.insert(
            "status".to_string(),
            serde_json::json!(metrics.status.as_str()),
        );
        fields.insert(
            "before_msg_count".to_string(),
            serde_json::json!(metrics.before_msg_count),
        );
        fields.insert(
            "before_token_count".to_string(),
            serde_json::json!(metrics.before_token_count),
        );
        // Unlike the durable payload, absent `after_*` counts become JSON null here:
        // an in-band event has a fixed shape, so consumers must not have to probe.
        fields.insert(
            "after_msg_count".to_string(),
            serde_json::json!(metrics.after_msg_count),
        );
        fields.insert(
            "after_token_count".to_string(),
            serde_json::json!(metrics.after_token_count),
        );
    }
    agentscope_core::event::AgentEvent::with_fields(
        agentscope_core::event::AgentEventType::Custom,
        fields,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::middleware::MiddlewareChain;
    use agentscope_core::model::GenerateOptions;

    fn state_with_history() -> Arc<Mutex<AgentState>> {
        let state = Arc::new(Mutex::new(AgentState::default()));
        let mut guard = state.lock().unwrap();
        for i in 0..6 {
            guard
                .context_mut()
                .push(Msg::user(format!("turn {i}: {}", "x".repeat(1_000))));
        }
        drop(guard);
        state
    }

    #[tokio::test]
    async fn a_long_turn_compacts_before_each_reasoning_step() {
        let state = state_with_history();
        let history = state.lock().unwrap().context().to_vec();
        let (delta_tx, mut delta_rx) = futures::channel::mpsc::unbounded();
        let ctx = RuntimeContext::new()
            .with_agent_id("agent")
            .with_session_id("s1");
        let ctx = RuntimeContext {
            state: Some(Arc::clone(&state)),
            delta_tx: Some(delta_tx),
            ..ctx
        };

        let middleware = ConversationCompactionMiddleware::new(
            Arc::new(ConversationCompactor::new(None)),
            CompactionConfig::builder()
                .trigger_messages(0)
                .trigger_tokens(100)
                .keep_messages(1)
                .build(),
            "agent",
        );
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(middleware));

        let seen: Arc<Mutex<Vec<Msg>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&seen);
        let core: Next<ReasoningInput> = Box::new(move |input: ReasoningInput| -> EventStream {
            *captured.lock().unwrap() = input.messages;
            Box::pin(futures::stream::once(async {
                Ok(agentscope_core::event::AgentEvent::ModelCallEnd(
                    Default::default(),
                ))
            }))
        });
        let mut messages = vec![Msg::system("frozen system prompt")];
        messages.extend(history);
        let input = ReasoningInput::new(messages, Vec::new(), GenerateOptions::default());
        let events: Vec<_> = chain.wrap_reasoning(ctx, input, core).collect().await;
        assert!(events.iter().all(|event| event.is_ok()), "{events:?}");

        // The model receives the compacted history, and the shared state is rewritten.
        let rendered = seen
            .lock()
            .unwrap()
            .iter()
            .map(Msg::text_content)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("frozen system prompt"), "{rendered}");
        assert!(rendered.contains("summary unavailable"), "{rendered}");
        assert!(rendered.contains("turn 5:"), "{rendered}");
        assert!(!rendered.contains("turn 0:"), "{rendered}");
        assert!(state.lock().unwrap().context().len() <= 2);

        let marker = delta_rx
            .next()
            .await
            .expect("compaction marker")
            .expect("ok event");
        assert_eq!(
            marker
                .data()
                .fields
                .get("kind")
                .and_then(|value| value.as_str()),
            Some("compaction")
        );
    }

    #[tokio::test]
    async fn provider_usage_can_trigger_compaction_when_the_local_count_is_low() {
        let state = Arc::new(Mutex::new(AgentState::default()));
        {
            let mut guard = state.lock().unwrap();
            guard.context_mut().push(Msg::user("hello"));
            let mut assistant = Msg::assistant("world");
            assistant.usage = Some(agentscope_core::message::ChatUsage {
                input_tokens: 50_000,
                output_tokens: 10,
                ..Default::default()
            });
            guard.context_mut().push(assistant);
        }
        let ctx = RuntimeContext {
            state: Some(Arc::clone(&state)),
            ..RuntimeContext::new().with_agent_id("agent")
        };
        let middleware = ConversationCompactionMiddleware::new(
            Arc::new(ConversationCompactor::new(None)),
            CompactionConfig::builder()
                .trigger_messages(0)
                .trigger_tokens(40_000)
                .keep_messages(1)
                .build(),
            "agent",
        );
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(middleware));

        let seen: Arc<Mutex<Vec<Msg>>> = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&seen);
        let core: Next<ReasoningInput> = Box::new(move |input: ReasoningInput| -> EventStream {
            *captured.lock().unwrap() = input.messages;
            Box::pin(futures::stream::once(async {
                Ok(agentscope_core::event::AgentEvent::ModelCallEnd(
                    Default::default(),
                ))
            }))
        });
        let history = state.lock().unwrap().context().to_vec();
        let input = ReasoningInput::new(history, Vec::new(), GenerateOptions::default());
        let events: Vec<_> = chain.wrap_reasoning(ctx, input, core).collect().await;
        assert!(events.iter().all(|event| event.is_ok()), "{events:?}");

        let rendered = seen
            .lock()
            .unwrap()
            .iter()
            .map(Msg::text_content)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("summary unavailable"), "{rendered}");
        // The retained tail's old usage must not anchor the next request.
        assert!(state
            .lock()
            .unwrap()
            .context()
            .iter()
            .all(|message| message.usage.is_none()));
    }

    #[tokio::test]
    async fn below_the_threshold_reasoning_is_untouched() {
        let state = Arc::new(Mutex::new(AgentState::default()));
        state.lock().unwrap().context_mut().push(Msg::user("short"));
        let ctx = RuntimeContext {
            state: Some(Arc::clone(&state)),
            ..RuntimeContext::new().with_agent_id("agent")
        };
        let middleware = ConversationCompactionMiddleware::new(
            Arc::new(ConversationCompactor::new(None)),
            CompactionConfig::builder()
                .trigger_messages(0)
                .trigger_tokens(10_000)
                .build(),
            "agent",
        );
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(middleware));
        let captured: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let seen = Arc::clone(&captured);
        let core: Next<ReasoningInput> = Box::new(move |input: ReasoningInput| -> EventStream {
            *seen.lock().unwrap() = input.messages.len();
            Box::pin(futures::stream::once(async {
                Ok(agentscope_core::event::AgentEvent::ModelCallEnd(
                    Default::default(),
                ))
            }))
        });
        let input = ReasoningInput::new(
            vec![Msg::system("sys"), Msg::user("short")],
            Vec::new(),
            GenerateOptions::default(),
        );
        let events: Vec<_> = chain.wrap_reasoning(ctx, input, core).collect().await;
        assert!(events.iter().all(|event| event.is_ok()), "{events:?}");
        assert_eq!(*captured.lock().unwrap(), 2);
        assert_eq!(state.lock().unwrap().context().len(), 1);
    }
}
