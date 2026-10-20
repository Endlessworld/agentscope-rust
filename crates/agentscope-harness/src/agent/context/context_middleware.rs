//! The middleware that puts the context compiler on the real model-call path.
//!
//! `ReActAgent` builds every reasoning request through `MiddlewareChain::wrap_model_call`, so
//! this is the last point where the request can still be assembled from material and fitted
//! into the budget. Registering the compiler here (rather than duplicating it inside the
//! agent) keeps a single compiler and a single manifest.

use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::middleware::{EventStream, ExtensionPoint, Middleware, ModelCallInput, Next};
use futures::StreamExt;

use super::builder::ContextPipeline;
use super::item::ContextPurpose;
use super::source::ContextRequest;

/// Applies a [`ContextPipeline`] to every model call.
pub struct ContextMiddleware {
    pipeline: Arc<ContextPipeline>,
}

impl ContextMiddleware {
    /// Creates the middleware around a shared pipeline.
    pub fn new(pipeline: Arc<ContextPipeline>) -> Self {
        Self { pipeline }
    }

    /// The wrapped pipeline.
    pub fn pipeline(&self) -> &Arc<ContextPipeline> {
        &self.pipeline
    }
}

impl std::fmt::Debug for ContextMiddleware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ContextMiddleware")
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
        let model = Arc::clone(&input.model);
        let ModelCallInput {
            messages,
            tools,
            options,
            model: _,
        } = input;

        let fut = async move {
            match pipeline
                .prepare(model.clone(), request, messages, tools, options)
                .await
            {
                // The continuation receives the compiled request, never the original one.
                Ok(prepared) => next(ModelCallInput::new(
                    prepared.messages,
                    prepared.tools,
                    prepared.options,
                    model,
                )),
                Err(error) => {
                    Box::pin(futures::stream::once(async move { Err(error) })) as EventStream
                }
            }
        };
        Box::pin(futures::stream::once(fut).flatten())
    }

    fn active_points(&self) -> Vec<ExtensionPoint> {
        vec![ExtensionPoint::OnModelCall]
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
        let mut chain = MiddlewareChain::new();
        chain.add(Box::new(middleware));
        let ctx = RuntimeContext::new()
            .with_user_id("alice")
            .with_session_id("s1")
            .with_agent_id("agent");
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

    #[test]
    fn the_middleware_only_participates_at_the_model_call_point() {
        let middleware = ContextMiddleware::new(pipeline(false));
        assert_eq!(middleware.active_points(), vec![ExtensionPoint::OnModelCall]);
        // The instruction is registered even without a dynamic source.
        assert!(!middleware.pipeline().sources().is_empty());
    }
}
