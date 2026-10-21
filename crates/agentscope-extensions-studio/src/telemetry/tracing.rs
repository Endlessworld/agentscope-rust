//! OTel GenAI span producer, mirroring Java's `TelemetryTracingMiddleware`.
//!
//! Java attaches tracing as a `MiddlewareBase` implementation active at three points
//! (`ON_AGENT`, `ON_MODEL_CALL`, `ON_ACTING`); this module does the same with
//! [`TelemetryTracing`]. Span lifetime is **RAII**: the span guard lives inside the
//! returned stream, so a stream that is dropped (a timeout, an abort, a lost channel
//! lease) still ends its span. That is what makes the cancellation seam from GAP-19
//! observable here without any `on_cancel` hook.
//!
//! [`TraceExporter::export`] is async while middleware hooks are synchronous, so
//! finished spans go through [`SpanSink`] — an unbounded queue drained by a background
//! task, the equivalent of Java's `BatchSpanProcessor`. Hosts that need determinism
//! (tests, short-lived processes) call [`TelemetryTracing::flush`].
//!
//! [`TraceExporter::export`]: super::exporter::TraceExporter::export

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::event::AgentEvent;
use agentscope_core::middleware::{
    ActingInput, AgentInput, EventStream, ExtensionPoint, Middleware, ModelCallInput, Next,
};
use chrono::Utc;
use futures::Stream;
use tokio::sync::{mpsc, oneshot};

use super::exporter::{unix_nanos, Span, SpanKind, SpanStatus, TraceExporter};
use super::extractors::{
    get_agent_request_attributes, get_common_attributes, get_llm_request_attributes,
    get_tool_request_attributes,
};
use super::gen_ai_attributes::operation_name;

/// Provider name recorded on model-call spans when the host does not override it.
///
/// The core `Model` trait exposes the model name but not the provider, so the value is
/// configuration, mirroring Java's `TelemetryTracer.Builder.providerName`.
pub const DEFAULT_PROVIDER_NAME: &str = "unknown";

/// Produces `invoke_agent` / `chat` / `execute_tool` spans for one agent.
///
/// One instance is registered in the agent's [`MiddlewareChain`] and shared by every run
/// of that agent; per-run state (the current span) is keyed by the runtime context's run
/// id, so concurrent runs never see each other's parent.
///
/// [`MiddlewareChain`]: agentscope_core::middleware::MiddlewareChain
pub struct TelemetryTracing {
    inner: Arc<TracingInner>,
}

/// Shared state, so one instance can be cloned into the agent's middleware chain *and*
/// into a host that flushes it.
struct TracingInner {
    sink: SpanSink,
    spans: Arc<Mutex<HashMap<String, SpanCtx>>>,
    provider_name: String,
}

impl Clone for TelemetryTracing {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl std::fmt::Debug for TelemetryTracing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryTracing")
            .field("provider_name", &self.inner.provider_name)
            .field(
                "active_runs",
                &self.inner.spans.lock().map(|s| s.len()).unwrap_or(0),
            )
            .finish()
    }
}

/// The span a nested phase attaches to.
#[derive(Debug, Clone)]
struct SpanCtx {
    trace_id: String,
    span_id: String,
}

impl TelemetryTracing {
    /// Creates a producer that exports through `exporter`.
    ///
    /// Must be called inside a Tokio runtime: the sink drains on a background task.
    pub fn new(exporter: Arc<dyn TraceExporter>) -> Self {
        Self {
            inner: Arc::new(TracingInner {
                sink: SpanSink::new(exporter),
                spans: Arc::new(Mutex::new(HashMap::new())),
                provider_name: DEFAULT_PROVIDER_NAME.to_string(),
            }),
        }
    }

    /// Overrides the provider name recorded on model-call spans.
    pub fn with_provider_name(mut self, provider_name: impl Into<String>) -> Self {
        // The instance is not shared yet, so the state is uniquely owned here.
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.provider_name = provider_name.into();
        }
        self
    }

    /// Waits until every span recorded so far has been handed to the exporter.
    pub async fn flush(&self) {
        self.inner.sink.flush().await;
    }

    /// The key one run's span context is stored under.
    ///
    /// The run id is the canonical key; session and agent ids are fallbacks for ad-hoc
    /// contexts that never went through `AgentRun` (tool calls, tests).
    fn run_key(ctx: &RuntimeContext) -> Option<String> {
        ctx.run_id
            .as_deref()
            .map(str::to_string)
            .or_else(|| ctx.session_id.as_deref().map(|id| format!("session:{id}")))
            .or_else(|| ctx.agent_id.as_deref().map(|id| format!("agent:{id}")))
    }

    fn current(&self, ctx: &RuntimeContext) -> Option<SpanCtx> {
        let key = Self::run_key(ctx)?;
        self.inner.spans.lock().ok()?.get(&key).cloned()
    }

    fn begin(
        &self,
        name: impl Into<String>,
        trace_id: impl Into<String>,
        span_id: impl Into<String>,
        parent_span_id: Option<String>,
        attributes: Vec<(String, serde_json::Value)>,
        kind: SpanKind,
    ) -> SpanGuard {
        let mut span = Span::new(name, trace_id, span_id);
        span.kind = kind;
        span.start_time_unix_nano = unix_nanos(Utc::now());
        span.attributes = attributes;
        span.parent_span_id = parent_span_id;
        SpanGuard {
            span,
            sink: self.inner.sink.clone(),
            finished: false,
        }
    }
}

impl Middleware for TelemetryTracing {
    fn on_agent(
        &self,
        ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        let trace_id = new_trace_id();
        let span_id = new_span_id();
        let mut attributes = get_common_attributes();
        attributes.extend(get_agent_request_attributes(
            ctx.agent_id.as_deref().unwrap_or_default(),
            ctx.agent_name.as_deref().unwrap_or_default(),
            None,
            &input.messages,
        ));
        let guard = self.begin(
            operation_name::INVOKE_AGENT,
            trace_id.clone(),
            span_id.clone(),
            None,
            attributes,
            SpanKind::Internal,
        );
        // Publish the run's root span *before* the inner stream is built: the phases run
        // inside `next`, so they must be able to find their parent immediately.
        let run_key = TelemetryTracing::run_key(ctx);
        if let Some(key) = &run_key {
            if let Ok(mut spans) = self.inner.spans.lock() {
                spans.insert(key.clone(), SpanCtx { trace_id, span_id });
            }
        }
        let inner = next(input);
        Box::pin(SpanStream::new(
            inner,
            vec![guard],
            run_key.map(|key| (Arc::clone(&self.inner.spans), key)),
        ))
    }

    fn on_model_call(
        &self,
        ctx: &RuntimeContext,
        input: ModelCallInput,
        next: Next<ModelCallInput>,
    ) -> EventStream {
        let parent = self.current(ctx);
        let trace_id = parent
            .as_ref()
            .map(|p| p.trace_id.clone())
            .unwrap_or_else(new_trace_id);
        let span_id = new_span_id();
        let mut attributes = get_common_attributes();
        attributes.extend(get_llm_request_attributes(
            &self.inner.provider_name,
            input.model.model_name(),
            &input.messages,
            &input.tools,
            Some(&input.options),
        ));
        let guard = self.begin(
            format!("{} {}", operation_name::CHAT, input.model.model_name()),
            trace_id,
            span_id,
            parent.map(|p| p.span_id),
            attributes,
            SpanKind::Client,
        );
        Box::pin(SpanStream::new(next(input), vec![guard], None))
    }

    fn on_acting(
        &self,
        ctx: &RuntimeContext,
        input: ActingInput,
        next: Next<ActingInput>,
    ) -> EventStream {
        let parent = self.current(ctx);
        let tool_calls = input.tool_calls.clone();
        let guards = tool_calls
            .iter()
            .map(|call| {
                let trace_id = parent
                    .as_ref()
                    .map(|p| p.trace_id.clone())
                    .unwrap_or_else(new_trace_id);
                let mut attributes = get_common_attributes();
                attributes.extend(get_tool_request_attributes(
                    ctx.toolkit.as_ref(),
                    &call.id,
                    &call.name,
                    &call.input,
                ));
                self.begin(
                    format!("{} {}", operation_name::EXECUTE_TOOL, call.name),
                    trace_id,
                    new_span_id(),
                    parent.as_ref().map(|p| p.span_id.clone()),
                    attributes,
                    SpanKind::Internal,
                )
            })
            .collect();
        Box::pin(SpanStream::new(next(input), guards, None))
    }

    fn active_points(&self) -> Vec<ExtensionPoint> {
        vec![
            ExtensionPoint::OnAgent,
            ExtensionPoint::OnModelCall,
            ExtensionPoint::OnActing,
        ]
    }
}

/// Ends a span exactly once, on the completion **or** the disposal path.
struct SpanGuard {
    span: Span,
    sink: SpanSink,
    finished: bool,
}

impl SpanGuard {
    fn end(&mut self, status: SpanStatus) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.span.end_time_unix_nano = unix_nanos(Utc::now());
        self.span.status = status;
        self.sink.record(self.span.clone());
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        // An unfinished guard means the stream was dropped without completing.
        self.end(SpanStatus::Unset);
    }
}

/// A stream that owns the spans of the phase it wraps.
///
/// Dropping the stream drops the guards, which ends the spans. This is the whole
/// cancellation story: no hook has to fire for a span to be closed.
struct SpanStream {
    inner: EventStream,
    guards: Vec<SpanGuard>,
    /// The run this stream belongs to, removed from the shared map when the run ends.
    run: Option<(Arc<Mutex<HashMap<String, SpanCtx>>>, String)>,
    finished: bool,
}

impl SpanStream {
    fn new(
        inner: EventStream,
        guards: Vec<SpanGuard>,
        run: Option<(Arc<Mutex<HashMap<String, SpanCtx>>>, String)>,
    ) -> Self {
        Self {
            inner,
            guards,
            run,
            finished: false,
        }
    }

    fn finish(&mut self, status: SpanStatus) {
        if self.finished {
            return;
        }
        self.finished = true;
        for guard in &mut self.guards {
            guard.end(status.clone());
        }
        if let Some((spans, key)) = self.run.take() {
            if let Ok(mut spans) = spans.lock() {
                spans.remove(&key);
            }
        }
    }
}

impl Stream for SpanStream {
    type Item = agentscope_core::error::Result<AgentEvent>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(None) => {
                this.finish(SpanStatus::Ok);
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(error))) => {
                // The run failed: close the spans here, and keep reporting the item.
                this.finish(SpanStatus::Error(error.to_string()));
                Poll::Ready(Some(Err(error)))
            }
            other => other,
        }
    }
}

impl Drop for SpanStream {
    fn drop(&mut self) {
        // Dropped mid-flight (timeout, abort, lost lease): the spans still end.
        self.finish(SpanStatus::Ok);
    }
}

/// Serializes finished spans to an async exporter without blocking the middleware.
#[derive(Clone)]
struct SpanSink {
    tx: mpsc::UnboundedSender<Batch>,
    closed: bool,
}

enum Batch {
    Span(Box<Span>),
    Flush(oneshot::Sender<()>),
}

impl SpanSink {
    fn new(exporter: Arc<dyn TraceExporter>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Batch>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let mut batch: Vec<Span> = Vec::new();
                    while let Some(message) = rx.recv().await {
                        match message {
                            Batch::Span(span) => batch.push(*span),
                            Batch::Flush(ack) => {
                                export(&exporter, &mut batch).await;
                                let _ = ack.send(());
                            }
                        }
                    }
                    // The producer is gone: flush what is left.
                    export(&exporter, &mut batch).await;
                });
                Self { tx, closed: false }
            }
            Err(_) => {
                // No runtime to drain the queue: spans are dropped rather than leaked.
                tracing::warn!("studio telemetry: no tokio runtime, spans will be dropped");
                drop(rx);
                Self { tx, closed: true }
            }
        }
    }

    fn record(&self, span: Span) {
        if self.closed {
            return;
        }
        let _ = self.tx.send(Batch::Span(Box::new(span)));
    }

    async fn flush(&self) {
        if self.closed {
            return;
        }
        let (ack, done) = oneshot::channel();
        if self.tx.send(Batch::Flush(ack)).is_ok() {
            let _ = done.await;
        }
    }
}

async fn export(exporter: &Arc<dyn TraceExporter>, batch: &mut Vec<Span>) {
    if batch.is_empty() {
        return;
    }
    let spans = std::mem::take(batch);
    if let Err(error) = exporter.export(&spans).await {
        tracing::warn!("studio telemetry export failed: {error}");
    }
}

/// A random 16-byte trace id (32 hex characters).
fn new_trace_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// A random 8-byte span id (16 hex characters).
fn new_span_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::event::AgentEventType;
    use agentscope_core::message::{Msg, ToolUseBlock};
    use futures::StreamExt;

    use super::super::exporter::RecordingTraceExporter;

    fn ctx(run_id: Option<&str>) -> RuntimeContext {
        let mut ctx = RuntimeContext::default().with_agent_id("assistant");
        ctx.agent_name = Some("assistant".to_string());
        ctx.run_id = run_id.map(str::to_string);
        ctx
    }

    fn agent_input() -> AgentInput {
        AgentInput::new(vec![Msg::user("hi")])
    }

    /// A `next` continuation that yields `events` and completes.
    fn next_with(
        events: Vec<agentscope_core::error::Result<AgentEvent>>,
    ) -> Next<AgentInput> {
        Box::new(move |_input: AgentInput| Box::pin(futures::stream::iter(events)))
    }

    async fn drain(stream: EventStream) -> Vec<agentscope_core::error::Result<AgentEvent>> {
        stream.collect().await
    }

    #[tokio::test]
    async fn a_completed_run_exports_the_agent_span() {
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone());
        let stream = tracing.on_agent(
            &ctx(Some("run-1")),
            agent_input(),
            next_with(vec![Ok(AgentEvent::new(AgentEventType::AgentStart))]),
        );
        let events = drain(stream).await;
        assert_eq!(events.len(), 1);

        tracing.flush().await;
        let spans = exporter.spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, operation_name::INVOKE_AGENT);
        assert_eq!(spans[0].status, SpanStatus::Ok);
        assert_eq!(spans[0].trace_id.len(), 32);
        assert_eq!(spans[0].span_id.len(), 16);
        assert!(spans[0].parent_span_id.is_none());
        assert!(spans[0].end_time_unix_nano >= spans[0].start_time_unix_nano);
    }

    #[tokio::test]
    async fn a_failed_run_marks_the_span_as_error() {
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone());
        let stream = tracing.on_agent(
            &ctx(Some("run-2")),
            agent_input(),
            next_with(vec![Err(agentscope_core::error::Error::Agent("boom".into()))]),
        );
        let _ = drain(stream).await;

        tracing.flush().await;
        let spans = exporter.spans();
        assert_eq!(spans.len(), 1);
        match &spans[0].status {
            SpanStatus::Error(message) => assert!(message.contains("boom"), "{message}"),
            other => panic!("expected an error status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_dropped_run_still_ends_the_span() {
        // GAP-19: cancellation is "drop the future". The span must close anyway, which
        // only holds if the guard travels inside the stream.
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone());
        let stream = tracing.on_agent(
            &ctx(Some("run-3")),
            agent_input(),
            next_with(vec![Ok(AgentEvent::new(AgentEventType::AgentStart))]),
        );
        // Drop without ever polling.
        drop(stream);

        tracing.flush().await;
        let spans = exporter.spans();
        assert_eq!(spans.len(), 1, "a dropped run must still export its span");
        assert_eq!(spans[0].name, operation_name::INVOKE_AGENT);
    }

    #[tokio::test]
    async fn nested_phases_attach_to_the_run_span() {
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone());
        let run_ctx = ctx(Some("run-4"));

        let model_ctx = run_ctx.clone();
        let acting_ctx = run_ctx.clone();
        let tracer = tracing.clone();
        let next: Next<AgentInput> = Box::new(move |_input: AgentInput| {
            let model = tracer.on_model_call(
                &model_ctx,
                model_input(),
                Box::new(|_| -> EventStream { Box::pin(futures::stream::empty()) }),
            );
            let acting = tracer.on_acting(
                &acting_ctx,
                ActingInput::new(vec![ToolUseBlock::new("c1", "search", Default::default())]),
                Box::new(|_| -> EventStream { Box::pin(futures::stream::empty()) }),
            );
            Box::pin(futures::stream::select_all(vec![model, acting]))
        });
        let _ = drain(tracing.on_agent(&run_ctx, agent_input(), next)).await;

        tracing.flush().await;
        let spans = exporter.spans();
        let root = spans
            .iter()
            .find(|s| s.name == operation_name::INVOKE_AGENT)
            .expect("root span");
        let chat = spans
            .iter()
            .find(|s| s.name.starts_with(operation_name::CHAT))
            .expect("model span");
        let tool = spans
            .iter()
            .find(|s| s.name.starts_with(operation_name::EXECUTE_TOOL))
            .expect("tool span");
        assert_eq!(chat.parent_span_id.as_deref(), Some(root.span_id.as_str()));
        assert_eq!(tool.parent_span_id.as_deref(), Some(root.span_id.as_str()));
        // OTel GenAI conventions: a model call is a client span, the rest are internal.
        assert_eq!(chat.kind, SpanKind::Client);
        assert_eq!(tool.kind, SpanKind::Internal);
        assert_eq!(chat.trace_id, root.trace_id);
        assert_eq!(tool.trace_id, root.trace_id);
    }

    #[tokio::test]
    async fn a_run_without_a_run_id_falls_back_to_the_session() {
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone());
        let mut ctx = ctx(None);
        ctx.session_id = Some("s-1".to_string());
        let _ = drain(tracing.on_agent(
            &ctx,
            agent_input(),
            next_with(vec![Ok(AgentEvent::new(AgentEventType::AgentStart))]),
        ))
        .await;
        tracing.flush().await;
        assert_eq!(exporter.len(), 1);
        // The fallback key is cleaned up like the run key.
        assert!(tracing.inner.spans.lock().unwrap().is_empty());
    }

    #[test]
    fn only_the_three_traced_points_are_active() {
        let tracing = TelemetryTracing::new(Arc::new(RecordingTraceExporter::new()));
        let points = tracing.active_points();
        assert_eq!(
            points,
            vec![
                ExtensionPoint::OnAgent,
                ExtensionPoint::OnModelCall,
                ExtensionPoint::OnActing
            ]
        );
    }

    /// A `Model` that answers with an empty stream: the model span needs only its name.
    struct StubModel;

    #[async_trait::async_trait]
    impl agentscope_core::model::Model for StubModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<agentscope_core::model::ToolSchema>,
            _options: agentscope_core::model::GenerateOptions,
        ) -> futures::stream::BoxStream<
            '_,
            agentscope_core::error::Result<agentscope_core::model::ChatResponse>,
        > {
            Box::pin(futures::stream::empty())
        }

        fn model_name(&self) -> &str {
            "gpt-4o-mini"
        }
    }

    /// A `Model` that answers with one text block, so a real agent run completes.
    struct AnsweringModel;

    #[async_trait::async_trait]
    impl agentscope_core::model::Model for AnsweringModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<agentscope_core::model::ToolSchema>,
            _options: agentscope_core::model::GenerateOptions,
        ) -> futures::stream::BoxStream<
            '_,
            agentscope_core::error::Result<agentscope_core::model::ChatResponse>,
        > {
            let response = agentscope_core::model::ChatResponse::builder()
                .content(vec![agentscope_core::message::ContentBlock::Text(
                    agentscope_core::message::TextBlock::new("hello"),
                )])
                .finish_reason("stop")
                .build();
            Box::pin(futures::stream::iter(vec![Ok(response)]))
        }

        fn model_name(&self) -> &str {
            "gpt-4o-mini"
        }
    }

    #[tokio::test]
    async fn a_real_agent_run_exports_agent_and_model_spans() {
        // The unit tests call the hooks directly; this one proves the middleware is wired
        // into a real run (the chain invokes it, and the runtime context carries the run id).
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone());
        let mut chain = agentscope_core::middleware::MiddlewareChain::new();
        chain.add(Box::new(tracing.clone()));

        let agent = agentscope_core::agent::ReActAgent::builder()
            .name("traced")
            .model(Arc::new(AnsweringModel))
            .middleware_chain(chain)
            .build()
            .expect("agent builds");

        let reply = agent
            .call(vec![Msg::user("hi")])
            .await
            .expect("call succeeds");
        assert_eq!(reply.text_content(), "hello");
        tracing.flush().await;

        let spans = exporter.spans();
        let root = spans
            .iter()
            .find(|s| s.name == operation_name::INVOKE_AGENT)
            .expect("agent span");
        let chat = spans
            .iter()
            .find(|s| s.name.starts_with(operation_name::CHAT))
            .expect("model span");
        assert_eq!(root.status, SpanStatus::Ok);
        assert_eq!(chat.parent_span_id.as_deref(), Some(root.span_id.as_str()));
        // The run leaves nothing behind in the shared map.
        assert!(tracing.inner.spans.lock().unwrap().is_empty());
    }

    fn model_input() -> ModelCallInput {
        ModelCallInput::new(
            vec![Msg::user("hi")],
            Vec::new(),
            agentscope_core::model::GenerateOptions::default(),
            Arc::new(StubModel),
        )
    }
}
