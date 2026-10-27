//! The middleware that puts the context compiler on the real model-call path.
//!
//! `ReActAgent` builds every reasoning request through `MiddlewareChain::wrap_model_call`, so
//! this is the last point where the request can still be assembled from material and fitted
//! into the budget. Registering the compiler here (rather than duplicating it inside the
//! agent) keeps a single compiler and a single manifest.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::message::{Msg, MsgRole};
use agentscope_core::middleware::{EventStream, ExtensionPoint, Middleware, ModelCallInput, Next};
use futures::StreamExt;

use super::builder::{BudgetExceeded, ContextPipeline, PrepareOutcome};
use super::item::ContextPurpose;
use super::source::ContextRequest;
use crate::agent::memory::compaction::{
    CompactionConfig, CompactionMetrics, ConversationCompactor,
};
use crate::agent::middleware::conversation_compaction::compaction_event;

/// Configuration of the budget-overflow fallback.
///
/// When the compiled request exceeds the model budget, the middleware compacts the
/// conversation with this compactor and retries instead of failing the model call.
#[derive(Clone)]
pub struct ContextOverflowCompaction {
    pub compactor: Arc<ConversationCompactor>,
    pub config: CompactionConfig,
}

/// The result of a budget-overflow compaction.
struct OverflowCompacted {
    /// Messages to compile again (`[system…] + [summary] + preserved tail`).
    messages: Vec<Msg>,
    /// The summary text announced to the client.
    summary: String,
    /// Quantified outcome for the in-band compaction event.
    metrics: CompactionMetrics,
}

/// Applies a [`ContextPipeline`] to every model call.
pub struct ContextMiddleware {
    pipeline: Arc<ContextPipeline>,
    overflow: Option<ContextOverflowCompaction>,
}

impl ContextMiddleware {
    /// Creates the middleware around a shared pipeline.
    pub fn new(pipeline: Arc<ContextPipeline>) -> Self {
        Self {
            pipeline,
            overflow: None,
        }
    }

    /// Installs the budget-overflow compaction fallback.
    ///
    /// A reasoning request that exceeds the local budget is logged as a warning, the
    /// conversation is compacted, and the request is compiled again. This is the last
    /// line of defence: proactive compaction still runs before the model call.
    pub fn with_overflow_compaction(
        mut self,
        compactor: Arc<ConversationCompactor>,
        config: CompactionConfig,
    ) -> Self {
        self.overflow = Some(ContextOverflowCompaction { compactor, config });
        self
    }

    /// The wrapped pipeline.
    pub fn pipeline(&self) -> &Arc<ContextPipeline> {
        &self.pipeline
    }
}

impl std::fmt::Debug for ContextMiddleware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextMiddleware")
            .field("overflow_compaction", &self.overflow.is_some())
            .finish_non_exhaustive()
    }
}

impl Middleware for ContextMiddleware {
    fn on_model_call(
        &self,
        ctx: &RuntimeContext,
        input: ModelCallInput,
        next: Next<ModelCallInput>,
    ) -> EventStream {
        // Every request routed through this chain is a reasoning preparation: auxiliary
        // (summary) calls use their own path and are compiled with `ContextPurpose::Summary`.
        let request = ContextRequest {
            agent_id: ctx.agent_id().map(str::to_string),
            user_id: ctx.user_id().map(str::to_string),
            session_id: ctx.session_id().map(str::to_string),
            call_id: uuid::Uuid::new_v4().to_string(),
            purpose: ContextPurpose::Reasoning,
        };
        let pipeline = Arc::clone(&self.pipeline);
        let overflow = self.overflow.clone();
        let model = Arc::clone(&input.model);
        // The committed task snapshot travels with the call: the compiler runs here, where no
        // agent state is reachable (core publishes the snapshot before the reasoning phase).
        let task_state = ctx
            .extension::<agentscope_core::state::TaskContextState>(
                agentscope_core::middleware::TASK_CONTEXT_STATE_CONTEXT_KEY,
            )
            .map(|state| (*state).clone());
        let state = ctx.state.clone();
        let agent_id = ctx.agent_id().unwrap_or("agent").to_string();
        let session_id = ctx.session_id().unwrap_or_default().to_string();
        // A recorder attached by the session scheduler makes the fallback compaction's
        // `compaction/start` and `compaction/end` durable session facts.
        let recorder = ctx.extension::<agentscope_core::session::SessionRecorder>(
            agentscope_core::session::RECORDER_CONTEXT_KEY,
        );
        // The agent-level stream merges fine-grained events from this channel, so the
        // fallback can announce its compaction to the ACP client just like the
        // proactive `on_agent` path does.
        let delta_tx = ctx.delta_tx.clone();
        let ModelCallInput {
            messages,
            tools,
            options,
            model: _,
        } = input;

        let fut = async move {
            let first = pipeline
                .prepare_with_task_outcome(
                    Arc::clone(&model),
                    request.clone(),
                    messages.clone(),
                    tools.clone(),
                    options.clone(),
                    task_state.as_ref(),
                )
                .await;
            let overflow_info: BudgetExceeded = match first {
                // The continuation receives the compiled request, never the original one.
                Ok(PrepareOutcome::Prepared(prepared)) => {
                    return next(ModelCallInput::new(
                        prepared.messages,
                        prepared.tools,
                        prepared.options,
                        model,
                    ))
                }
                Ok(PrepareOutcome::BudgetExceeded(info)) => info,
                Err(error) => {
                    return Box::pin(futures::stream::once(async move { Err(error) }))
                        as EventStream
                }
            };

            // The local budget miss is recoverable: log it as a warning (the ACP log
            // file) and compact instead of turning it into the agent's terminal error.
            tracing::warn!(
                agent = %agent_id,
                session = %session_id,
                detail = %overflow_info,
                "model input exceeds the local budget; compacting the conversation and retrying"
            );

            let Some(overflow) = overflow else {
                tracing::warn!(
                    agent = %agent_id,
                    session = %session_id,
                    "no compaction fallback is configured; surfacing the local budget failure"
                );
                let error = overflow_info.into_error();
                return Box::pin(futures::stream::once(async move { Err(error) }))
                    as EventStream;
            };

            let mut last_info = overflow_info;
            for summary_only in [false, true] {
                let Some(compacted) = compact_overflow(
                    state.clone(),
                    &messages,
                    &overflow,
                    &agent_id,
                    &session_id,
                    recorder.as_ref(),
                    summary_only,
                )
                .await
                else {
                    continue;
                };

                // Announce the compaction in-band so the ACP layer can translate it into a
                // capability-gated `compaction_update`. Without this, an overflow compaction
                // would shrink the context invisibly.
                if let Some(tx) = &delta_tx {
                    let _ = tx.unbounded_send(Ok(compaction_event(
                        &agent_id,
                        &compacted.summary,
                        Some(&compacted.metrics),
                    )));
                }

                let retry_request = ContextRequest {
                    call_id: uuid::Uuid::new_v4().to_string(),
                    ..request.clone()
                };
                match pipeline
                    .prepare_with_task_outcome(
                        Arc::clone(&model),
                        retry_request,
                        compacted.messages,
                        tools.clone(),
                        options.clone(),
                        task_state.as_ref(),
                    )
                    .await
                {
                    Ok(PrepareOutcome::Prepared(prepared)) => {
                        return next(ModelCallInput::new(
                            prepared.messages,
                            prepared.tools,
                            prepared.options,
                            model,
                        ))
                    }
                    Ok(PrepareOutcome::BudgetExceeded(info)) => {
                        tracing::warn!(
                            agent = %agent_id,
                            session = %session_id,
                            detail = %info,
                            summary_only,
                            "model input is still over budget after compaction"
                        );
                        last_info = info;
                    }
                    Err(error) => {
                        return Box::pin(futures::stream::once(async move { Err(error) }))
                            as EventStream
                    }
                }
            }

            tracing::warn!(
                agent = %agent_id,
                session = %session_id,
                "conversation compaction could not free enough context; surfacing the budget failure"
            );
            let error = last_info.into_error();
            Box::pin(futures::stream::once(async move { Err(error) })) as EventStream
        };
        Box::pin(futures::stream::once(fut).flatten())
    }

    fn active_points(&self) -> Vec<ExtensionPoint> {
        vec![ExtensionPoint::OnModelCall]
    }
}

/// Compacts the conversation after a local budget miss.
///
/// The history that is compacted is the request actually being compiled, not a snapshot of
/// the shared state: `on_reasoning` middlewares (tool-result eviction) may already have
/// rewritten it, and the retry must not resurrect what they removed. When the call runs
/// inside an agent the compacted history is also written back to the shared state, so the
/// next iteration and the next turn start from it; an ad-hoc call without shared state is
/// compacted locally for the retry only. Returns the messages to compile again, or `None`
/// when no compaction could free context.
async fn compact_overflow(
    state: Option<Arc<std::sync::Mutex<agentscope_core::state::AgentState>>>,
    messages: &[Msg],
    overflow: &ContextOverflowCompaction,
    agent_id: &str,
    session_id: &str,
    recorder: Option<&Arc<agentscope_core::session::SessionRecorder>>,
    summary_only: bool,
) -> Option<OverflowCompacted> {
    let history: Vec<Msg> = messages
        .iter()
        .filter(|message| message.role != MsgRole::System)
        .cloned()
        .collect();
    // The session id still comes from the durable state when one is attached; ad-hoc calls
    // fall back to the runtime context.
    let effective_session = match &state {
        Some(state) => state.lock().unwrap().session_id().to_string(),
        None => session_id.to_string(),
    };
    if history.is_empty() {
        return None;
    }

    // First keep the configured tail. If even that does not bound the result (the tail
    // itself is oversized), the caller retries with a summary-only history.
    let mut config = overflow.config.clone();
    config.trigger_messages = 1;
    config.trigger_tokens = 0;
    if summary_only {
        config.keep_messages = 0;
        config.keep_tokens = 0;
    } else {
        config.keep_messages = config
            .keep_messages
            .min(history.len().saturating_sub(1));
    }

    match overflow
        .compactor
        .compact_if_needed_with_metrics(
            history,
            &config,
            agent_id,
            &effective_session,
            recorder,
        )
        .await
    {
        Ok((Some(compacted), metrics)) => {
            tracing::info!(
                agent = %agent_id,
                session = %effective_session,
                status = metrics.status.as_str(),
                before_count = metrics.before_msg_count,
                after_count = metrics.after_msg_count.unwrap_or(0),
                summary_only,
                "conversation compacted after a local budget miss"
            );
            // The retained tail's usage was produced against the old prefix; the next
            // request must not anchor on it.
            let compacted = crate::agent::context::strip_provider_usage(compacted);
            let summary = compacted
                .first()
                .map(|message| message.text_content())
                .unwrap_or_default();
            if let Some(state) = &state {
                let mut guard = state.lock().unwrap();
                let system: Vec<Msg> = guard
                    .context()
                    .iter()
                    .filter(|message| message.role == MsgRole::System)
                    .cloned()
                    .collect();
                let mut new_context = system;
                new_context.extend(compacted.iter().cloned());
                *guard.context_mut() = new_context;
            }
            let mut retry: Vec<Msg> = messages
                .iter()
                .filter(|message| message.role == MsgRole::System)
                .cloned()
                .collect();
            retry.extend(compacted);
            Some(OverflowCompacted {
                messages: retry,
                summary,
                metrics,
            })
        }
        Ok((None, _)) => None,
        Err(error) => {
            tracing::warn!(
                agent = %agent_id,
                session = %effective_session,
                %error,
                "conversation compaction failed while resolving a local budget miss"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use agentscope_core::message::Msg;
    use agentscope_core::middleware::MiddlewareChain;
    use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
    use futures::stream::BoxStream;
    use futures::StreamExt;

    use super::super::builder::ContextPipelineConfig;
    use super::super::item::ContextBlock;
    use super::super::policy::{ApproximateEstimator, ContextPolicy};
    use super::super::source::{ContextSource, ContextSourceOptions, ContextSources};
    use super::*;

    struct StubModel {
        window: usize,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl Model for StubModel {
        fn stream(
            &self,
            messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, crate::Result<ChatResponse>> {
            let rendered: Vec<String> = messages.iter().map(|m| m.text_content()).collect();
            self.seen.lock().unwrap().extend(rendered);
            Box::pin(futures::stream::empty())
        }

        fn model_name(&self) -> &str {
            "stub"
        }

        fn context_window_size(&self) -> usize {
            self.window
        }
    }

    struct FixedSource(Vec<ContextBlock>);

    #[async_trait::async_trait]
    impl ContextSource for FixedSource {
        async fn load(&self, _request: &ContextRequest) -> crate::Result<Vec<ContextBlock>> {
            Ok(self.0.clone())
        }
    }

    /// A pipeline with an instruction and (optionally) one runtime source.
    fn pipeline(with_source: bool) -> Arc<ContextPipeline> {
        let mut sources = ContextSources::empty()
            .with_instruction("policy", "preserve the public API")
            .unwrap();
        if with_source {
            sources = sources
                .with_source(
                    "biz",
                    Arc::new(FixedSource(vec![
                        ContextBlock::runtime("status", "Change 42 under review").unwrap(),
                    ])),
                    ContextSourceOptions::default(),
                )
                .unwrap();
        }
        Arc::new(ContextPipeline::new(ContextPipelineConfig {
            sources,
            policy: ContextPolicy::new(
                0,
                0,
                0,
                Arc::new(ApproximateEstimator),
                Arc::new(|_| {}),
            ),
            ..ContextPipelineConfig::default()
        }))
    }

    /// Drives the real entry point: `MiddlewareChain::wrap_model_call`.
    async fn run(
        middleware: ContextMiddleware,
        model: Arc<dyn Model>,
        messages: Vec<Msg>,
    ) -> Vec<crate::Result<agentscope_core::event::AgentEvent>> {
        let ctx = RuntimeContext::new()
            .with_user_id("alice")
            .with_session_id("s1")
            .with_agent_id("agent");
        run_with_ctx(middleware, model, messages, ctx).await
    }

    /// Same as [`run`] with a caller-supplied runtime context (for shared state).
    async fn run_with_ctx(
        middleware: ContextMiddleware,
        model: Arc<dyn Model>,
        messages: Vec<Msg>,
        ctx: RuntimeContext,
    ) -> Vec<crate::Result<agentscope_core::event::AgentEvent>> {
        run_chain(vec![Box::new(middleware)], model, messages, ctx).await
    }

    /// Drives a chain of middlewares (outermost first) over the final model call.
    async fn run_chain(
        middlewares: Vec<Box<dyn Middleware>>,
        model: Arc<dyn Model>,
        messages: Vec<Msg>,
        ctx: RuntimeContext,
    ) -> Vec<crate::Result<agentscope_core::event::AgentEvent>> {
        let mut chain = MiddlewareChain::new();
        for middleware in middlewares {
            chain.add(middleware);
        }
        let input = ModelCallInput::new(messages, Vec::new(), GenerateOptions::default(), model);
        let core: Next<ModelCallInput> = Box::new(|input: ModelCallInput| -> EventStream {
            let model = Arc::clone(&input.model);
            let fut = async move {
                let mut stream = model.stream(input.messages, input.tools, input.options);
                while let Some(item) = stream.next().await {
                    let _ = item?;
                }
                Ok(agentscope_core::event::AgentEvent::ModelCallEnd(
                    Default::default(),
                ))
            };
            Box::pin(futures::stream::once(fut))
        });
        let stream = chain.wrap_model_call(ctx, input, core);
        stream.collect().await
    }

    #[tokio::test]
    async fn the_compiled_request_reaches_the_model() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let model: Arc<dyn Model> = Arc::new(StubModel {
            window: 128_000,
            seen: Arc::clone(&seen),
        });
        let events = run(
            ContextMiddleware::new(pipeline(true)),
            model,
            vec![Msg::user("investigate")],
        )
        .await;
        assert!(events.iter().all(|event| event.is_ok()));

        let rendered: String = seen.lock().unwrap().join("\n");
        // The instruction, the runtime material and the conversation all reach the provider.
        assert!(rendered.contains("preserve the public API"), "{rendered}");
        assert!(rendered.contains("Change 42 under review"), "{rendered}");
        assert!(rendered.contains("investigate"), "{rendered}");
    }

    #[tokio::test]
    async fn a_budget_failure_surfaces_instead_of_calling_the_model() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let model: Arc<dyn Model> = Arc::new(StubModel {
            window: 10,
            seen: Arc::clone(&seen),
        });
        let events = run(
            ContextMiddleware::new(pipeline(true)),
            model,
            vec![Msg::user("investigate")],
        )
        .await;
        // The instruction cannot be evicted, so preparation fails and no model call happens.
        assert!(events.iter().any(|event| event.is_err()));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_budget_overflow_compacts_the_state_and_retries() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let model: Arc<dyn Model> = Arc::new(StubModel {
            window: 1_000,
            seen: Arc::clone(&seen),
        });

        // A long history that cannot fit the 1k window. The first message is the one
        // compaction must summarize away.
        let state = Arc::new(Mutex::new(agentscope_core::state::AgentState::default()));
        {
            let mut guard = state.lock().unwrap();
            for i in 0..6 {
                guard.context_mut().push(Msg::user(format!(
                    "turn {i}: {}",
                    "x".repeat(1_200)
                )));
            }
        }
        // The reasoning request carries the accumulated state history (as `ReActAgent`
        // assembles it); it cannot fit the 1k window.
        let messages = state.lock().unwrap().context().to_vec();
        let (delta_tx, mut delta_rx) = futures::channel::mpsc::unbounded();
        let ctx = RuntimeContext::new()
            .with_user_id("alice")
            .with_session_id("s1")
            .with_agent_id("agent");
        let ctx = RuntimeContext {
            state: Some(Arc::clone(&state)),
            delta_tx: Some(delta_tx),
            ..ctx
        };

        let compactor = Arc::new(ConversationCompactor::new(None));
        let middleware = ContextMiddleware::new(pipeline(false)).with_overflow_compaction(
            compactor,
            CompactionConfig::builder().keep_messages(1).build(),
        );
        let events = run_with_ctx(middleware, model, messages, ctx).await;
        assert!(events.iter().all(|event| event.is_ok()), "{events:?}");

        // The ACP layer receives the compaction marker as an in-band event.
        let marker = delta_rx
            .next()
            .await
            .expect("compaction marker")
            .expect("ok event");
        assert_eq!(
            marker.data().fields.get("kind").and_then(|v| v.as_str()),
            Some("compaction"),
            "{marker:?}"
        );

        // The model received the summary plus the preserved tail, not the original history.
        let rendered = seen.lock().unwrap().join("\n");
        assert!(rendered.contains("summary unavailable"), "{rendered}");
        assert!(rendered.contains("turn 5:"), "{rendered}");
        assert!(!rendered.contains("turn 0:"), "{rendered}");

        // The durable state was rewritten too: the next iteration starts compacted.
        let guard = state.lock().unwrap();
        assert!(guard.context().len() <= 2, "{:?}", guard.context().len());
    }

    #[test]
    fn the_middleware_only_participates_at_the_model_call_point() {
        let middleware = ContextMiddleware::new(pipeline(false));
        assert_eq!(middleware.active_points(), vec![ExtensionPoint::OnModelCall]);
        // The instruction is registered even without a dynamic source.
        assert!(!middleware.pipeline().sources().is_empty());
    }

    /// A model that answers with a text reply, so tests can drive the real agent loop.
    struct AnsweringStubModel {
        window: usize,
        requests: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl Model for AnsweringStubModel {
        fn stream(
            &self,
            messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, crate::Result<ChatResponse>> {
            self.requests
                .lock()
                .unwrap()
                .push(messages.iter().map(|m| m.text_content()).collect());
            let response = ChatResponse::builder()
                .content(vec![agentscope_core::message::ContentBlock::Text(
                    agentscope_core::message::TextBlock::new("ack"),
                )])
                .build();
            Box::pin(futures::stream::once(async move { Ok(response) }))
        }

        fn model_name(&self) -> &str {
            "answering-stub"
        }

        fn context_window_size(&self) -> usize {
            self.window
        }
    }

    /// An outer model-call middleware that rewrites the request without touching the shared
    /// state — the shape `ToolResultEvictionMiddleware` produces at the reasoning boundary.
    struct DropFromRequest(&'static str);

    impl Middleware for DropFromRequest {
        fn on_model_call(
            &self,
            _ctx: &RuntimeContext,
            mut input: ModelCallInput,
            next: Next<ModelCallInput>,
        ) -> EventStream {
            input
                .messages
                .retain(|message| !message.text_content().contains(self.0));
            next(input)
        }

        fn active_points(&self) -> Vec<ExtensionPoint> {
            vec![ExtensionPoint::OnModelCall]
        }
    }

    #[tokio::test]
    async fn the_fallback_compacts_the_request_it_was_handed() {
        // The shared state still carries every message, but an earlier middleware already
        // dropped one from the request. Compacting the state snapshot would resurrect it in
        // the retried request (oversized again, and reintroducing content the agent evicted).
        let seen = Arc::new(Mutex::new(Vec::new()));
        let model: Arc<dyn Model> = Arc::new(StubModel {
            window: 1_000,
            seen: Arc::clone(&seen),
        });

        let state = Arc::new(Mutex::new(agentscope_core::state::AgentState::default()));
        {
            let mut guard = state.lock().unwrap();
            for i in 0..5 {
                guard.context_mut().push(Msg::user(format!(
                    "turn {i}: {}",
                    "x".repeat(1_200)
                )));
            }
            guard.context_mut().push(Msg::user(format!(
                "PRUNED-TAIL: {}",
                "x".repeat(1_200)
            )));
        }
        let messages: Vec<Msg> = state
            .lock()
            .unwrap()
            .context()
            .iter()
            .filter(|message| !message.text_content().contains("PRUNED-TAIL"))
            .cloned()
            .collect();
        let ctx = RuntimeContext {
            state: Some(Arc::clone(&state)),
            ..RuntimeContext::new()
                .with_user_id("alice")
                .with_session_id("s1")
                .with_agent_id("agent")
        };

        let middleware = ContextMiddleware::new(pipeline(false)).with_overflow_compaction(
            Arc::new(ConversationCompactor::new(None)),
            CompactionConfig::builder().keep_messages(1).build(),
        );
        let events = run_chain(
            vec![Box::new(DropFromRequest("PRUNED-TAIL")), Box::new(middleware)],
            model,
            messages,
            ctx,
        )
        .await;
        assert!(events.iter().all(|event| event.is_ok()), "{events:?}");

        let rendered = seen.lock().unwrap().join("\n");
        assert!(rendered.contains("summary unavailable"), "{rendered}");
        assert!(rendered.contains("turn 4:"), "{rendered}");
        assert!(
            !rendered.contains("PRUNED-TAIL"),
            "the retry must not resurrect a message the request already dropped: {rendered}"
        );
    }

    #[tokio::test]
    async fn the_agent_keeps_the_current_turn_and_persists_the_compaction() {
        // Reproduction of the real path: prior turns already exceed the window, and the
        // question of *this* turn is the tail compaction must preserve. The loop runs on a
        // private state clone, so this only holds if the loop hands the conversation over
        // before the model call and takes the rewrite back afterwards.
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model: Arc<dyn Model> = Arc::new(AnsweringStubModel {
            window: 1_000,
            requests: Arc::clone(&requests),
        });

        let mut seeded = agentscope_core::state::AgentState::default();
        for i in 0..6 {
            seeded.context_mut().push(Msg::user(format!(
                "turn {i}: {}",
                "x".repeat(1_200)
            )));
        }

        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(
            ContextMiddleware::new(pipeline(false)).with_overflow_compaction(
                Arc::new(ConversationCompactor::new(None)),
                CompactionConfig::builder().keep_messages(1).build(),
            ),
        ));
        let agent = agentscope_core::agent::ReActAgent::builder()
            .name("overflow-e2e")
            .model(model)
            .middleware_chain(chain)
            .state(seeded)
            .build()
            .unwrap();
        let state = agent.state_handle();

        agent
            .call(vec![Msg::user("CURRENT QUESTION")])
            .await
            .expect("the retried request must succeed");

        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 1, "one compaction, one retried model call");
            let rendered = requests[0].join("\n");
            assert!(rendered.contains("CURRENT QUESTION"), "{rendered}");
            assert!(rendered.contains("summary unavailable"), "{rendered}");
            assert!(!rendered.contains("turn 0:"), "{rendered}");
        }

        // The rewrite was taken back: the customary end-of-turn commit must not restore the
        // oversized history, and the next turn must not recompact from scratch.
        {
            let guard = state.lock().unwrap();
            let persisted: String = guard
                .context()
                .iter()
                .map(|message| message.text_content())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(persisted.contains("summary unavailable"), "{persisted}");
            assert!(!persisted.contains("turn 0:"), "{persisted}");
        }

        agent
            .call(vec![Msg::user("SECOND QUESTION")])
            .await
            .expect("the second turn must succeed");
        let requests = requests.lock().unwrap();
        let last = requests.last().unwrap().join("\n");
        assert!(last.contains("SECOND QUESTION"), "{last}");
        assert!(last.contains("summary unavailable"), "{last}");
        assert!(
            !last.contains("turn 0:"),
            "the compacted history must be what the next turn starts from: {last}"
        );
    }
}
