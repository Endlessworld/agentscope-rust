//! OTel GenAI span producer, mirroring Java's `TelemetryTracer`.
//!
//! Java attaches tracing as a `Tracer` implementation at four points (`callAgent`,
//! `callModel`, `callTool`, `callFormat`); this module does the same with
//! [`TelemetryTracing`]: all four are core [`Middleware`] hooks
//! (`OnAgent`/`OnModelCall`/`OnActing`/`OnFormat`). The `OnFormat` hook hands over the
//! requested [`ResponseFormat`] but neither the formatter instance nor its output, so the
//! span it produces records the request side only; hosts that format messages themselves can
//! still use [`TelemetryTracing::call_format`], the direct equivalent of Java's
//! `callFormat(formatter, msgs, supplier)`.
//!
//! [`ResponseFormat`]: agentscope_core::formatter::ResponseFormat
//!
//! Every span records both sides of the call, like Java:
//! - request attributes immediately (`get_*_request_attributes`),
//! - response attributes when the phase ends (`get_*_response_attributes`), which is why
//!   [`SpanGuard`] carries an optional finalizer. The model span is fed by a
//!   [`StreamChatResponseAggregator`] that tees the provider stream (Java's
//!   `doOnNext(aggregator::append)`), the tool span by the acting phase's result sink, and
//!   the agent span by the `AgentResult` event's `reply` payload.
//!
//! Span lifetime is **RAII**: the span guard lives inside the returned stream, so a stream
//! that is dropped (a timeout, an abort, a lost channel lease) still ends its span. That is
//! what makes the cancellation seam from GAP-19 observable here without any `on_cancel` hook.
//!
//! [`TraceExporter::export`] is async while middleware hooks are synchronous, so
//! finished spans go through [`SpanSink`] — an unbounded queue drained by a background
//! task, the equivalent of Java's `BatchSpanProcessor`. The task exports on a schedule
//! (`StudioConfig::telemetry_batch_interval`, default 5s) and as soon as a batch is full
//! (`StudioConfig::telemetry_batch_size`, default 512), so a long-lived agent exports without
//! leaking spans. A failed export keeps its batch and retries with backoff (100ms doubling,
//! capped at 5s, three attempts) instead of dropping the spans. Hosts that need determinism
//! (tests, short-lived processes) still call [`TelemetryTracing::flush`].
//!
//! [`TraceExporter::export`]: super::exporter::TraceExporter::export

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::event::AgentEvent;
use agentscope_core::formatter::ResponseFormat;
use agentscope_core::message::Msg;
use agentscope_core::middleware::{
    ActingInput, ActingResultsHandle, AgentInput, EventStream, ExtensionPoint, Middleware,
    ModelCallInput, Next,
};
use agentscope_core::model::{ChatResponse, GenerateOptions, Model, ToolSchema};
use chrono::Utc;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use tokio::sync::{mpsc, oneshot};

use super::agent_scope_attributes as as_attrs;
use super::aggregator::StreamChatResponseAggregator;
use super::exporter::{unix_nanos, Span, SpanKind, SpanStatus, TraceExporter};
use super::extractors::{
    get_agent_request_attributes, get_agent_response_attributes, get_common_attributes,
    get_format_request_attributes, get_format_response_attributes, get_function_name,
    get_llm_request_attributes, get_llm_response_attributes, get_tool_request_attributes,
    get_tool_response_attributes, Attributes, FormatterConverter,
};
use super::gen_ai_attributes::operation_name;
use crate::studio::config::{
    StudioConfig, DEFAULT_TELEMETRY_BATCH_INTERVAL, DEFAULT_TELEMETRY_BATCH_SIZE,
};

/// Provider name recorded on model-call spans when neither the model nor the host names one.
///
/// [`Model::provider_name`] is the primary source; this value is the fallback the host
/// configures through [`TelemetryTracing::with_provider_name`], mirroring Java's
/// `TelemetryTracer.Builder.providerName`.
pub const DEFAULT_PROVIDER_NAME: &str = "unknown";

/// Agent type name recorded in `agentscope.function.name` when the host does not
/// override it.
///
/// Java derives it reflectively (`instance.getClass().getSimpleName()`,
/// `AttributesExtractors.java:350`), which Rust cannot do for a `&RuntimeContext`-only
/// hook; the core's single agent implementation is `ReActAgent`.
pub const DEFAULT_AGENT_TYPE_NAME: &str = "ReActAgent";

/// Model type name used when the provider is unknown (Rust cannot reflect the type).
const FALLBACK_MODEL_TYPE_NAME: &str = "ChatModel";

/// Formatter type name used when the provider is unknown (Rust cannot reflect the type).
const FALLBACK_FORMATTER_TYPE_NAME: &str = "ChatFormatter";

/// The `agentscope.function.name` value of a tool span.
///
/// Java: `getFunctionName(instance, "callTool")` with the `Toolkit` instance
/// (`TelemetryTracer.java:169`) → `Toolkit.callTool`.
pub const TOOL_FUNCTION_NAME: &str = "Toolkit.callTool";

/// Java class-like name for a provider, used to build the model span's
/// `agentscope.function.name` the way `getFunctionName(instance, "callModel")` would
/// (`TelemetryTracer.java:123`).
fn model_type_name(provider_name: &str) -> &'static str {
    match provider_name {
        super::gen_ai_attributes::provider_name::OPENAI => "OpenAIChatModel",
        as_attrs::provider_name::DASHSCOPE => "DashScopeChatModel",
        super::gen_ai_attributes::provider_name::GCP_GEMINI => "GeminiChatModel",
        super::gen_ai_attributes::provider_name::ANTHROPIC => "AnthropicChatModel",
        _ => FALLBACK_MODEL_TYPE_NAME,
    }
}

/// Java formatter simple class name for a provider, used by the framework `format` span.
///
/// Java reads it off the formatter instance it wraps (`getFunctionName(formatter, "format")`,
/// `TelemetryTracer.java:213`); core's `OnFormat` hook only hands over the requested format, so
/// the name is derived from the provider family — the same input the model span uses.
fn formatter_type_name(provider_name: &str) -> &'static str {
    match provider_name {
        super::gen_ai_attributes::provider_name::OPENAI => "OpenAIChatFormatter",
        as_attrs::provider_name::DASHSCOPE => "DashScopeChatFormatter",
        super::gen_ai_attributes::provider_name::GCP_GEMINI => "GeminiChatFormatter",
        super::gen_ai_attributes::provider_name::ANTHROPIC => "AnthropicChatFormatter",
        _ => FALLBACK_FORMATTER_TYPE_NAME,
    }
}

/// The `(key, value)` pair for `agentscope.function.name`.
fn function_name_attribute(value: String) -> (String, serde_json::Value) {
    (
        as_attrs::AGENTSCOPE_FUNCTION_NAME.to_string(),
        serde_json::Value::String(value),
    )
}

/// The agent span name: `invoke_agent <agent name>`, like Java's
/// `INVOKE_AGENT + " " + instance.getName()` (`TelemetryTracer.java:96`).
fn agent_span_name(agent_name: Option<&str>) -> String {
    match agent_name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => format!("{} {name}", operation_name::INVOKE_AGENT),
        None => operation_name::INVOKE_AGENT.to_string(),
    }
}

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
    agent_type: String,
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
    /// Batching follows the OTel SDK defaults (5s schedule delay, 512 spans per batch); use
    /// [`TelemetryTracing::with_config`] to take those knobs from a [`StudioConfig`].
    ///
    /// Must be called inside a Tokio runtime: the sink drains on a background task.
    pub fn new(exporter: Arc<dyn TraceExporter>) -> Self {
        Self::with_batch_config(exporter, BatchConfig::default())
    }

    /// Creates a producer whose batched export follows `config`
    /// (`telemetry_batch_interval` / `telemetry_batch_size`).
    pub fn with_config(exporter: Arc<dyn TraceExporter>, config: &StudioConfig) -> Self {
        Self::with_batch_config(exporter, BatchConfig::from_config(config))
    }

    fn with_batch_config(exporter: Arc<dyn TraceExporter>, batch_config: BatchConfig) -> Self {
        Self {
            inner: Arc::new(TracingInner {
                sink: SpanSink::new(exporter, batch_config),
                spans: Arc::new(Mutex::new(HashMap::new())),
                provider_name: DEFAULT_PROVIDER_NAME.to_string(),
                agent_type: DEFAULT_AGENT_TYPE_NAME.to_string(),
            }),
        }
    }

    /// Overrides the provider name recorded on model-call spans.
    ///
    /// [`Model::provider_name`] wins when the model reports one; this value is the fallback for
    /// models that keep the trait default (`unknown`). It must be applied before the tracer is
    /// shared (the instance is cloned into the middleware chain *and* kept by the host), because
    /// the state lives behind an `Arc`: a late call cannot take effect and is logged instead of
    /// being dropped silently.
    pub fn with_provider_name(mut self, provider_name: impl Into<String>) -> Self {
        let provider_name = provider_name.into();
        // The instance is not shared yet, so the state is uniquely owned here.
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.provider_name = provider_name,
            None => tracing::warn!(
                provider_name = %provider_name,
                "studio telemetry: provider name ignored — the tracer is already shared; set it before cloning the tracer into the middleware chain"
            ),
        }
        self
    }

    /// Overrides the agent type name recorded in `agentscope.function.name`.
    ///
    /// Java reads the concrete class name reflectively; a host that wraps or renames the
    /// agent can pass its own type name here. As with [`TelemetryTracing::with_provider_name`],
    /// a call on an already-shared tracer cannot take effect and is logged.
    pub fn with_agent_type_name(mut self, agent_type_name: impl Into<String>) -> Self {
        let agent_type_name = agent_type_name.into();
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.agent_type = agent_type_name,
            None => tracing::warn!(
                agent_type_name = %agent_type_name,
                "studio telemetry: agent type name ignored — the tracer is already shared; set it before cloning the tracer into the middleware chain"
            ),
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

    /// The provider name recorded on model spans: what the model reports, else the configured
    /// fallback (STUDIO-13).
    ///
    /// Java reflects the model class (`ProviderNameConverter`); the core trait now lets a model
    /// answer that question itself, so the configured value is only needed for models that keep
    /// the `unknown` default.
    fn resolved_provider_name(&self, model: &dyn Model) -> String {
        let reported = model.provider_name().trim();
        if reported.is_empty() || reported == DEFAULT_PROVIDER_NAME {
            self.inner.provider_name.clone()
        } else {
            reported.to_string()
        }
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
            finalizer: None,
        }
    }

    /// Runs `format` inside a `format <target>` span and records both sides of the call.
    ///
    /// Rust equivalent of Java's `TelemetryTracer.callFormat` (`TelemetryTracer.java:203`):
    /// Java wraps the synchronous formatter invocation in a span, records
    /// `getFormatRequestAttributes` up front and `getFormatResponseAttributes` on success.
    /// Core's [`ExtensionPoint::OnFormat`] hook already produces the format span for every
    /// structured-output call (see `on_format`), but it carries no formatter instance and no
    /// output — a host that formats messages itself with a known formatter keeps using this
    /// wrapper for the full both-sides span.
    ///
    /// `formatter_type` is the formatter's simple class name (`"OpenAIChatFormatter"`, …); it
    /// is mapped to the span's target through [`FormatterConverter::get_formatter_target`]
    /// exactly like Java's `FormatterConverter`. `ctx` supplies the parent span when a run
    /// context is at hand; pass `None` for a root format span (Java's `Context.current()`
    /// outside a traced call).
    ///
    /// A panic in `format` unwinds with the span still open; the guard's `Drop` ends it with an
    /// unset status, mirroring Java's `finally { span.end(); }` without inventing an
    /// exception status Java does not set.
    pub fn call_format<T: serde::Serialize>(
        &self,
        ctx: Option<&RuntimeContext>,
        formatter_type: &str,
        msgs: &[Msg],
        format: impl FnOnce() -> T,
    ) -> T {
        let parent = ctx.and_then(|ctx| self.current(ctx));
        let trace_id = parent
            .as_ref()
            .map(|p| p.trace_id.clone())
            .unwrap_or_else(new_trace_id);
        let target = FormatterConverter::get_formatter_target(formatter_type);
        let mut attributes = get_common_attributes();
        attributes.extend(get_format_request_attributes(target, Some(msgs)));
        attributes.push(function_name_attribute(get_function_name(
            formatter_type,
            "format",
        )));
        let mut guard = self.begin(
            format!("{} {target}", as_attrs::operation_name::FORMAT),
            trace_id,
            new_span_id(),
            parent.map(|p| p.span_id),
            attributes,
            SpanKind::Internal,
        );
        let result = format();
        guard
            .span
            .attributes
            .extend(get_format_response_attributes(&result));
        guard.end(SpanStatus::Ok);
        result
    }
}

#[async_trait::async_trait]
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
        // Java `TelemetryTracer.java:104`: `AGENTSCOPE_FUNCTION_NAME` = `<AgentType>.callAgent`.
        attributes.push(function_name_attribute(get_function_name(
            self.inner.agent_type.as_str(),
            "callAgent",
        )));
        // Java `doOnSuccess(msg -> span.setAllAttributes(getAgentResponseAttributes(msg)))`:
        // the reply only exists once the run produced its `AgentResult` event.
        let reply: Arc<Mutex<Option<Msg>>> = Arc::new(Mutex::new(None));
        let guard = self
            .begin(
                agent_span_name(ctx.agent_name.as_deref()),
                trace_id.clone(),
                span_id.clone(),
                None,
                attributes,
                SpanKind::Internal,
            )
            .with_finalizer({
                let reply = Arc::clone(&reply);
                move || {
                    reply
                        .lock()
                        .ok()
                        .and_then(|slot| slot.clone())
                        .map(|msg| get_agent_response_attributes(&msg))
                        .unwrap_or_default()
                }
            });
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
            Some(reply),
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
        // STUDIO-13: the model knows its family; the configured name is only the fallback.
        let provider_name = self.resolved_provider_name(input.model.as_ref());
        let mut attributes = get_common_attributes();
        attributes.extend(get_llm_request_attributes(
            &provider_name,
            input.model.model_name(),
            &input.messages,
            &input.tools,
            Some(&input.options),
        ));
        // Java `TelemetryTracer.java:124`: `<ChatModelType>.callModel`.
        attributes.push(function_name_attribute(get_function_name(
            model_type_name(&provider_name),
            "callModel",
        )));
        // Java feeds every streamed chunk into a `StreamChatResponseAggregator` and records
        // `getLLMResponseAttributes(aggregator.getResponse())` in `doFinally`. Rust's
        // `on_model_call` stream only carries `ModelCallEnd`, so the provider stream itself is
        // teed instead — that is the same observation point Java uses.
        let aggregate = Arc::new(Mutex::new(StreamChatResponseAggregator::create()));
        let guard = self
            .begin(
                format!("{} {}", operation_name::CHAT, input.model.model_name()),
                trace_id,
                span_id,
                parent.map(|p| p.span_id),
                attributes,
                SpanKind::Client,
            )
            .with_finalizer({
                let aggregate = Arc::clone(&aggregate);
                move || {
                    aggregate
                        .lock()
                        .ok()
                        .map(|aggregate| {
                            let response = aggregate.response();
                            get_llm_response_attributes(Some(&response))
                        })
                        .unwrap_or_default()
                }
            });
        let input = ModelCallInput::new(
            input.messages,
            input.tools,
            input.options,
            Arc::new(AggregatingModel::new(input.model, aggregate)),
        );
        Box::pin(SpanStream::new(next(input), vec![guard], None, None))
    }

    fn on_acting(
        &self,
        ctx: &RuntimeContext,
        input: ActingInput,
        next: Next<ActingInput>,
    ) -> EventStream {
        let parent = self.current(ctx);
        let tool_calls = input.tool_calls.clone();
        // Java `doOnSuccess(result -> span.setAllAttributes(getToolResponseAttributes(result)))`:
        // the acting phase collects its results into this handle, so it is the response source.
        let results = input.results.clone();
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
                attributes.push(function_name_attribute(TOOL_FUNCTION_NAME.to_string()));
                self.begin(
                    format!("{} {}", operation_name::EXECUTE_TOOL, call.name),
                    trace_id,
                    new_span_id(),
                    parent.as_ref().map(|p| p.span_id.clone()),
                    attributes,
                    SpanKind::Internal,
                )
                .with_finalizer(tool_result_attributes(results.clone(), call.id.clone()))
            })
            .collect();
        Box::pin(SpanStream::new(next(input), guards, None, None))
    }

    fn active_points(&self) -> Vec<ExtensionPoint> {
        vec![
            ExtensionPoint::OnAgent,
            ExtensionPoint::OnModelCall,
            ExtensionPoint::OnActing,
            ExtensionPoint::OnFormat,
        ]
    }

    /// Records the structured-output request handed over by the framework (STUDIO-12).
    ///
    /// Java's `callFormat` wraps the formatter invocation (`TelemetryTracer.java:203-226`);
    /// core's [`ExtensionPoint::OnFormat`] seam runs before each model request is built and
    /// hands over the requested [`ResponseFormat`], but neither the formatter instance nor its
    /// output. The span therefore carries the request side (operation, target, function name)
    /// and ends immediately with the same unset status Java's `finally { span.end() }` leaves
    /// when nothing set one. Nothing is recorded for an unconstrained call, exactly like Java,
    /// where `callFormat` only runs when a format is requested.
    async fn on_format(&self, ctx: &RuntimeContext, format: &mut Option<ResponseFormat>) {
        if format.is_none() {
            return;
        }
        let parent = self.current(ctx);
        let trace_id = parent
            .as_ref()
            .map(|p| p.trace_id.clone())
            .unwrap_or_else(new_trace_id);
        // Java maps the formatter class name through `FormatterConverter`; the provider family
        // is the closest identity this hook exposes.
        let formatter_type = formatter_type_name(&self.inner.provider_name);
        let target = FormatterConverter::get_formatter_target(formatter_type);
        let mut attributes = get_common_attributes();
        attributes.extend(get_format_request_attributes(target, None));
        attributes.push(function_name_attribute(get_function_name(
            formatter_type,
            "format",
        )));
        let mut guard = self.begin(
            format!("{} {target}", as_attrs::operation_name::FORMAT),
            trace_id,
            new_span_id(),
            parent.map(|p| p.span_id),
            attributes,
            SpanKind::Internal,
        );
        guard.end(SpanStatus::Unset);
    }
}

/// Ends a span exactly once, on the completion **or** the disposal path.
struct SpanGuard {
    span: Span,
    sink: SpanSink,
    finished: bool,
    /// Attributes that can only be computed when the phase ends (Java's `doOnSuccess` /
    /// `doFinally` response attributes).
    finalizer: Option<Box<dyn FnOnce() -> Attributes + Send>>,
}

impl SpanGuard {
    /// Attaches the response-side attributes computed at end time.
    fn with_finalizer(mut self, finalizer: impl FnOnce() -> Attributes + Send + 'static) -> Self {
        self.finalizer = Some(Box::new(finalizer));
        self
    }

    fn end(&mut self, status: SpanStatus) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Some(finalizer) = self.finalizer.take() {
            // A cancelled (dropped) phase ends here too: Java's `doFinally` also records the
            // response attributes it managed to aggregate.
            self.span.attributes.extend(finalizer());
        }
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
    /// The agent run's final reply, captured from its `AgentResult` event.
    reply: Option<Arc<Mutex<Option<Msg>>>>,
    finished: bool,
}

impl SpanStream {
    fn new(
        inner: EventStream,
        guards: Vec<SpanGuard>,
        run: Option<(Arc<Mutex<HashMap<String, SpanCtx>>>, String)>,
        reply: Option<Arc<Mutex<Option<Msg>>>>,
    ) -> Self {
        Self {
            inner,
            guards,
            run,
            reply,
            finished: false,
        }
    }

    /// Records the agent's final message, the value Java's `callAgent` receives in
    /// `doOnSuccess`.
    fn capture_reply(&self, event: &AgentEvent) {
        let Some(reply) = &self.reply else {
            return;
        };
        let AgentEvent::AgentResult(data) = event else {
            return;
        };
        let Some(value) = data.fields.get("reply") else {
            return;
        };
        if let Ok(msg) = serde_json::from_value::<Msg>(value.clone()) {
            if let Ok(mut slot) = reply.lock() {
                *slot = Some(msg);
            }
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

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
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
            Poll::Ready(Some(Ok(event))) => {
                this.capture_reply(&event);
                Poll::Ready(Some(Ok(event)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for SpanStream {
    fn drop(&mut self) {
        // Dropped mid-flight (timeout, abort, lost lease): the spans still end.
        self.finish(SpanStatus::Ok);
    }
}

/// Builds the end-of-phase finalizer that records a tool span's response attributes.
///
/// Java records `getToolResponseAttributes(result)` on success
/// (`TelemetryTracer.java:190`). Rust's acting phase writes each `ToolResultBlock` into
/// [`ActingResultsHandle`] (first writer wins), which is the equivalent of the value Java's
/// `doOnSuccess` receives — the event stream itself only carries `ToolResultEnd` state, never
/// the output blocks.
fn tool_result_attributes(
    results: ActingResultsHandle,
    tool_call_id: String,
) -> impl FnOnce() -> Attributes + Send + 'static {
    move || {
        results
            .snapshot()
            .results
            .iter()
            .find(|block| block.id.as_deref() == Some(tool_call_id.as_str()))
            .map(|block| get_tool_response_attributes(&block.output))
            .unwrap_or_default()
    }
}

/// A model wrapper that feeds every streamed chunk into a
/// [`StreamChatResponseAggregator`], mirroring Java's
/// `.doOnNext(aggregator::append)`.
///
/// `on_model_call` only sees `ModelCallEnd` (the text/tool deltas are routed to the agent
/// stream through `RuntimeContext.delta_tx`), so the aggregator has to observe the provider
/// stream directly. Every other capability is forwarded untouched.
struct AggregatingModel {
    inner: Arc<dyn Model>,
    aggregate: Arc<Mutex<StreamChatResponseAggregator>>,
}

impl AggregatingModel {
    fn new(inner: Arc<dyn Model>, aggregate: Arc<Mutex<StreamChatResponseAggregator>>) -> Self {
        Self { inner, aggregate }
    }
}

impl Model for AggregatingModel {
    fn stream(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
    ) -> BoxStream<'_, agentscope_core::error::Result<ChatResponse>> {
        // One `stream()` call per attempt: start clean so a retried attempt replaces the
        // abandoned one instead of appending to it (Java has no model-level retry here).
        if let Ok(mut aggregate) = self.aggregate.lock() {
            *aggregate = StreamChatResponseAggregator::create();
        }
        let aggregate = Arc::clone(&self.aggregate);
        Box::pin(
            self.inner
                .stream(messages, tools, options)
                .map(move |chunk| {
                    if let Ok(chunk) = &chunk {
                        if let Ok(mut aggregate) = aggregate.lock() {
                            aggregate.append(chunk);
                        }
                    }
                    chunk
                }),
        )
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn supports_native_structured_output(&self) -> bool {
        self.inner.supports_native_structured_output()
    }

    fn supports_native_structured_output_with_tools(&self) -> bool {
        self.inner.supports_native_structured_output_with_tools()
    }

    fn supports_tool_choice_specific(&self) -> bool {
        self.inner.supports_tool_choice_specific()
    }

    fn context_window_size(&self) -> usize {
        self.inner.context_window_size()
    }

    fn max_output_tokens(&self) -> usize {
        self.inner.max_output_tokens()
    }

    fn descriptor(&self) -> Option<&agentscope_core::model::catalog::ModelDescriptor> {
        self.inner.descriptor()
    }
}

/// Batching knobs for the background span exporter.
///
/// Mirrors the OTel SDK `BatchSpanProcessor` options Java installs in
/// `TelemetryTracer.Builder#build` (`scheduleDelay` + `maxExportBatchSize`), which is what makes
/// a long-lived `TelemetryTracing` export at all (STUDIO-02).
#[derive(Debug, Clone, Copy)]
struct BatchConfig {
    /// How often finished spans are flushed even when the batch is not full.
    interval: Duration,
    /// Number of buffered spans that triggers an immediate export.
    max_batch_size: usize,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            interval: DEFAULT_TELEMETRY_BATCH_INTERVAL,
            max_batch_size: DEFAULT_TELEMETRY_BATCH_SIZE,
        }
    }
}

impl BatchConfig {
    fn from_config(config: &StudioConfig) -> Self {
        Self {
            // `tokio::time::interval` panics on a zero period, and a zero batch size would
            // export span-by-span; clamp both instead of trusting the caller.
            interval: config
                .telemetry_batch_interval()
                .max(Duration::from_millis(1)),
            max_batch_size: config.telemetry_batch_size().max(1),
        }
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
    fn new(exporter: Arc<dyn TraceExporter>, batch_config: BatchConfig) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Batch>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let mut batch: Vec<Span> = Vec::new();
                    // First tick fires one interval from now, like `BatchSpanProcessor`'s
                    // repeating schedule (not immediately).
                    let mut ticker = tokio::time::interval_at(
                        tokio::time::Instant::now() + batch_config.interval,
                        batch_config.interval,
                    );
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        tokio::select! {
                            message = rx.recv() => match message {
                                Some(Batch::Span(span)) => {
                                    batch.push(*span);
                                    // A full batch is exported right away, so the buffer is
                                    // bounded even when spans arrive faster than the schedule.
                                    if batch.len() >= batch_config.max_batch_size {
                                        export(&exporter, &mut batch).await;
                                    }
                                }
                                Some(Batch::Flush(ack)) => {
                                    export(&exporter, &mut batch).await;
                                    let _ = ack.send(());
                                }
                                None => {
                                    // The producer is gone: flush what is left.
                                    export(&exporter, &mut batch).await;
                                    break;
                                }
                            },
                            // Only the periodic tick: `export` early-returns on an empty
                            // batch, so an idle tracer costs nothing.
                            _ = ticker.tick() => export(&exporter, &mut batch).await,
                        }
                    }
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

/// How often one batch is offered to the exporter before it is dropped.
///
/// The OTel SDK `BatchSpanProcessor` retries through its exporter instead of dropping on the
/// first failure (STUDIO-06); three attempts keep a permanently broken endpoint from wedging
/// the sink while surviving a transient outage.
const EXPORT_MAX_ATTEMPTS: u32 = 3;

/// First retry delay; doubles per attempt, capped by [`EXPORT_RETRY_MAX_DELAY`].
const EXPORT_RETRY_BASE_DELAY: Duration = Duration::from_millis(100);

/// Upper bound of the retry backoff (same cap as [`crate::studio::StudioClient`]).
const EXPORT_RETRY_MAX_DELAY: Duration = Duration::from_secs(5);

async fn export(exporter: &Arc<dyn TraceExporter>, batch: &mut Vec<Span>) {
    if batch.is_empty() {
        return;
    }
    // Keep the spans across attempts: taking them out of the buffer and dropping them on a
    // failed export is exactly the loss this retry exists to prevent.
    let spans = std::mem::take(batch);
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match exporter.export(&spans).await {
            Ok(()) => return,
            Err(error) if attempt < EXPORT_MAX_ATTEMPTS => {
                let delay = EXPORT_RETRY_BASE_DELAY
                    .saturating_mul(1u32 << (attempt - 1))
                    .min(EXPORT_RETRY_MAX_DELAY);
                tracing::warn!(
                    attempt,
                    spans = spans.len(),
                    ?delay,
                    "studio telemetry export failed, retrying: {error}"
                );
                tokio::time::sleep(delay).await;
            }
            Err(error) => {
                tracing::warn!(
                    attempt,
                    spans = spans.len(),
                    "studio telemetry export failed, giving up on this batch: {error}"
                );
                return;
            }
        }
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
    use crate::telemetry::gen_ai_attributes as gen_ai;

    fn ctx(run_id: Option<&str>) -> RuntimeContext {
        let mut ctx = RuntimeContext::default().with_agent_id("assistant");
        ctx.agent_name = Some("assistant".to_string());
        ctx.run_id = run_id.map(str::to_string);
        ctx
    }

    fn agent_input() -> AgentInput {
        AgentInput::new(vec![Msg::user("hi")])
    }

    /// The span name the default `ctx()` produces: `invoke_agent <agent name>`.
    fn expected_agent_span_name() -> String {
        format!("{} assistant", operation_name::INVOKE_AGENT)
    }

    fn attr_value<'a>(span: &'a Span, key: &str) -> Option<&'a serde_json::Value> {
        span.attributes
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    fn attr_str<'a>(span: &'a Span, key: &str) -> &'a str {
        attr_value(span, key)
            .and_then(|value| value.as_str())
            .unwrap_or_else(|| panic!("span {} has no string attribute {key}", span.name))
    }

    /// A `next` continuation that yields `events` and completes.
    fn next_with(events: Vec<agentscope_core::error::Result<AgentEvent>>) -> Next<AgentInput> {
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
        assert_eq!(spans[0].name, expected_agent_span_name());
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
            next_with(vec![Err(agentscope_core::error::Error::Agent(
                "boom".into(),
            ))]),
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
        assert_eq!(spans[0].name, expected_agent_span_name());
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
            .find(|s| s.name == expected_agent_span_name())
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
                .id("resp-real")
                .content(vec![agentscope_core::message::ContentBlock::Text(
                    agentscope_core::message::TextBlock::new("hello"),
                )])
                .usage(
                    agentscope_core::message::ChatUsage::builder()
                        .input_tokens(4)
                        .output_tokens(2)
                        .build(),
                )
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
            .find(|s| s.name == format!("{} traced", operation_name::INVOKE_AGENT))
            .expect("agent span");
        let chat = spans
            .iter()
            .find(|s| s.name.starts_with(operation_name::CHAT))
            .expect("model span");
        assert_eq!(root.status, SpanStatus::Ok);
        assert_eq!(chat.parent_span_id.as_deref(), Some(root.span_id.as_str()));
        // Java `callAgent`: `<AgentType>.callAgent` + the reply as response attributes.
        assert_eq!(
            attr_str(root, as_attrs::AGENTSCOPE_FUNCTION_NAME),
            "ReActAgent.callAgent"
        );
        assert!(attr_str(root, gen_ai::GEN_AI_OUTPUT_MESSAGES).contains("hello"));
        // Java `callModel`: `<ChatModelType>.callModel` + the aggregated response, which only
        // exists if the provider stream was teed into the aggregator on the real agent path.
        assert_eq!(
            attr_str(chat, as_attrs::AGENTSCOPE_FUNCTION_NAME),
            "ChatModel.callModel"
        );
        assert_eq!(
            attr_value(chat, gen_ai::GEN_AI_USAGE_INPUT_TOKENS),
            Some(&serde_json::json!(4))
        );
        assert_eq!(
            attr_value(chat, gen_ai::GEN_AI_USAGE_OUTPUT_TOKENS),
            Some(&serde_json::json!(2))
        );
        assert_eq!(
            attr_value(chat, gen_ai::GEN_AI_RESPONSE_FINISH_REASONS),
            Some(&serde_json::json!(["stop"]))
        );
        assert_eq!(attr_str(chat, gen_ai::GEN_AI_RESPONSE_ID), "resp-real");
        // The run leaves nothing behind in the shared map.
        assert!(tracing.inner.spans.lock().unwrap().is_empty());
    }

    #[test]
    fn function_names_follow_the_java_type_and_method_shape() {
        // Java `AttributesExtractors.getFunctionName(instance, method)` =
        // `instance.getClass().getSimpleName() + "." + method` (AttributesExtractors.java:350).
        assert_eq!(
            get_function_name(DEFAULT_AGENT_TYPE_NAME, "callAgent"),
            "ReActAgent.callAgent"
        );
        assert_eq!(
            get_function_name(model_type_name("openai"), "callModel"),
            "OpenAIChatModel.callModel"
        );
        assert_eq!(
            get_function_name(model_type_name("dashscope"), "callModel"),
            "DashScopeChatModel.callModel"
        );
        assert_eq!(
            get_function_name(model_type_name("gcp.gemini"), "callModel"),
            "GeminiChatModel.callModel"
        );
        assert_eq!(
            get_function_name(model_type_name("anthropic"), "callModel"),
            "AnthropicChatModel.callModel"
        );
        // Rust cannot reflect the provider: unknown providers keep a generic type name.
        assert_eq!(
            get_function_name(model_type_name("unknown"), "callModel"),
            "ChatModel.callModel"
        );
        assert_eq!(TOOL_FUNCTION_NAME, "Toolkit.callTool");
    }

    #[tokio::test]
    async fn the_agent_span_records_the_reply_as_response_attributes() {
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone());
        let reply = Msg::assistant("the final reply");
        let mut data = agentscope_core::event::EventData::default();
        data.fields
            .insert("reply".to_string(), serde_json::json!(reply));
        let _ = drain(tracing.on_agent(
            &ctx(Some("run-reply")),
            agent_input(),
            next_with(vec![
                Ok(AgentEvent::new(AgentEventType::AgentStart)),
                Ok(AgentEvent::AgentResult(data)),
            ]),
        ))
        .await;
        tracing.flush().await;

        let spans = exporter.spans();
        assert_eq!(spans.len(), 1);
        // Java `doOnSuccess(msg -> span.setAllAttributes(getAgentResponseAttributes(msg)))`.
        assert!(attr_str(&spans[0], gen_ai::GEN_AI_OUTPUT_MESSAGES).contains("the final reply"));
        assert!(
            attr_str(&spans[0], as_attrs::AGENTSCOPE_FUNCTION_OUTPUT).contains("the final reply")
        );
    }

    /// A `Model` that streams two chunks, so the aggregator has something to merge.
    struct StreamingModel;

    #[async_trait::async_trait]
    impl agentscope_core::model::Model for StreamingModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<agentscope_core::model::ToolSchema>,
            _options: agentscope_core::model::GenerateOptions,
        ) -> futures::stream::BoxStream<
            '_,
            agentscope_core::error::Result<agentscope_core::model::ChatResponse>,
        > {
            let first = agentscope_core::model::ChatResponse::builder()
                .id("resp-stream")
                .content(vec![agentscope_core::message::ContentBlock::Text(
                    agentscope_core::message::TextBlock::new("Hel"),
                )])
                .usage(
                    agentscope_core::message::ChatUsage::builder()
                        .input_tokens(9)
                        .output_tokens(1)
                        .cached_tokens(5)
                        .cache_creation_tokens(7)
                        .reasoning_tokens(9)
                        .tool_use_prompt_tokens(3)
                        .build(),
                )
                .build();
            let second = agentscope_core::model::ChatResponse::builder()
                .id("resp-stream")
                .content(vec![agentscope_core::message::ContentBlock::Text(
                    agentscope_core::message::TextBlock::new("lo"),
                )])
                .usage(
                    agentscope_core::message::ChatUsage::builder()
                        .input_tokens(9)
                        .output_tokens(5)
                        .build(),
                )
                .finish_reason("stop")
                .build();
            Box::pin(futures::stream::iter(vec![Ok(first), Ok(second)]))
        }

        fn model_name(&self) -> &str {
            "gpt-4o-mini"
        }
    }

    #[tokio::test]
    async fn the_model_span_aggregates_the_streamed_response() {
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone()).with_provider_name("openai");
        let input = ModelCallInput::new(
            vec![Msg::user("hi")],
            Vec::new(),
            agentscope_core::model::GenerateOptions::default(),
            Arc::new(StreamingModel),
        );
        // Drive the wrapped model the way the agent core does: consume its stream.
        let next: Next<ModelCallInput> = Box::new(|input: ModelCallInput| {
            let model = Arc::clone(&input.model);
            let fut = async move {
                let mut stream = model.stream(input.messages, input.tools, input.options);
                let mut events = Vec::new();
                while let Some(chunk) = stream.next().await {
                    chunk.expect("streamed chunk");
                    events.push(Ok(AgentEvent::new(AgentEventType::ModelCallEnd)));
                }
                events
            };
            Box::pin(futures::stream::once(fut).flat_map(futures::stream::iter))
        });
        let events = drain(tracing.on_model_call(&ctx(Some("run-stream")), input, next)).await;
        assert_eq!(events.len(), 2);
        tracing.flush().await;

        let spans = exporter.spans();
        assert_eq!(spans.len(), 1);
        let chat = &spans[0];
        assert_eq!(chat.name, "chat gpt-4o-mini");
        assert_eq!(
            attr_value(chat, gen_ai::GEN_AI_USAGE_INPUT_TOKENS),
            Some(&serde_json::json!(9))
        );
        // Usage is cumulative per chunk: Java takes the max.
        assert_eq!(
            attr_value(chat, gen_ai::GEN_AI_USAGE_OUTPUT_TOKENS),
            Some(&serde_json::json!(5))
        );
        // STUDIO-04: cache / cache-creation / reasoning / tool-use usage reaches the span
        // instead of being recorded as 0.
        assert_eq!(
            attr_value(chat, gen_ai::GEN_AI_USAGE_CACHE_READ_INPUT_TOKENS),
            Some(&serde_json::json!(5))
        );
        assert_eq!(
            attr_value(chat, gen_ai::GEN_AI_USAGE_CACHE_CREATION_INPUT_TOKENS),
            Some(&serde_json::json!(7))
        );
        assert_eq!(
            attr_value(chat, gen_ai::GEN_AI_USAGE_REASONING_OUTPUT_TOKENS),
            Some(&serde_json::json!(9))
        );
        assert_eq!(
            attr_value(chat, as_attrs::AGENTSCOPE_USAGE_TOOL_USE_PROMPT_TOKENS),
            Some(&serde_json::json!(3))
        );
        assert_eq!(
            attr_value(chat, gen_ai::GEN_AI_RESPONSE_FINISH_REASONS),
            Some(&serde_json::json!(["stop"]))
        );
        assert_eq!(attr_str(chat, gen_ai::GEN_AI_RESPONSE_ID), "resp-stream");
        // The chunks are merged, not concatenated: "Hel" + "lo" is one text part.
        assert!(attr_str(chat, gen_ai::GEN_AI_OUTPUT_MESSAGES).contains("Hello"));
        assert_eq!(
            attr_str(chat, as_attrs::AGENTSCOPE_FUNCTION_NAME),
            "OpenAIChatModel.callModel"
        );
        assert!(attr_str(chat, as_attrs::AGENTSCOPE_FUNCTION_OUTPUT).contains("resp-stream"));
    }

    #[tokio::test]
    async fn the_tool_span_records_the_result_from_the_acting_sink() {
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone());
        let call = ToolUseBlock::new("call-1", "search", Default::default());
        let input = ActingInput::new(vec![call.clone()]);
        // The acting core writes its results into the handle: that is the response side.
        let results = input.results.clone();
        let next: Next<ActingInput> = Box::new(move |_input: ActingInput| {
            results.record_placeholder(&call, "found it");
            Box::pin(futures::stream::empty())
        });
        let _ = drain(tracing.on_acting(&ctx(Some("run-tool")), input, next)).await;
        tracing.flush().await;

        let spans = exporter.spans();
        assert_eq!(spans.len(), 1);
        let tool = &spans[0];
        assert_eq!(tool.name, "execute_tool search");
        assert_eq!(
            attr_str(tool, as_attrs::AGENTSCOPE_FUNCTION_NAME),
            TOOL_FUNCTION_NAME
        );
        assert!(attr_str(tool, gen_ai::GEN_AI_TOOL_CALL_RESULT).contains("found it"));
        assert!(attr_str(tool, as_attrs::AGENTSCOPE_FUNCTION_OUTPUT).contains("found it"));
    }

    #[tokio::test]
    async fn call_format_emits_a_format_span_with_both_sides() {
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone());
        let formatted =
            tracing.call_format(None, "OpenAIChatFormatter", &[Msg::user("hi")], || {
                vec!["formatted-1".to_string()]
            });
        assert_eq!(formatted, vec!["formatted-1".to_string()]);
        tracing.flush().await;

        let spans = exporter.spans();
        assert_eq!(spans.len(), 1);
        let format = &spans[0];
        // Java `FormatterConverter.getFormatterTarget("OpenAIChatFormatter")` → dashscope.
        assert_eq!(format.name, "format dashscope");
        assert_eq!(format.kind, SpanKind::Internal);
        assert_eq!(format.status, SpanStatus::Ok);
        assert!(format.parent_span_id.is_none());
        assert_eq!(
            attr_str(format, gen_ai::GEN_AI_OPERATION_NAME),
            as_attrs::operation_name::FORMAT
        );
        assert_eq!(
            attr_str(format, as_attrs::AGENTSCOPE_FORMAT_TARGET),
            "dashscope"
        );
        assert_eq!(
            attr_str(format, as_attrs::AGENTSCOPE_FUNCTION_NAME),
            "OpenAIChatFormatter.format"
        );
        assert!(attr_str(format, as_attrs::AGENTSCOPE_FUNCTION_INPUT).contains("hi"));
        assert!(attr_str(format, as_attrs::AGENTSCOPE_FUNCTION_OUTPUT).contains("formatted-1"));
    }

    #[tokio::test]
    async fn call_format_attaches_to_the_run_span() {
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone());
        let run_ctx = ctx(Some("run-format"));
        let inner_ctx = run_ctx.clone();
        let tracer = tracing.clone();
        let next: Next<AgentInput> = Box::new(move |_| {
            tracer.call_format(
                Some(&inner_ctx),
                "AnthropicChatFormatter",
                &[Msg::user("hi")],
                || "ok",
            );
            Box::pin(futures::stream::empty())
        });
        let _ = drain(tracing.on_agent(&run_ctx, agent_input(), next)).await;
        tracing.flush().await;

        let spans = exporter.spans();
        let root = spans
            .iter()
            .find(|s| s.name == expected_agent_span_name())
            .expect("agent span");
        let format = spans
            .iter()
            .find(|s| s.name.starts_with(as_attrs::operation_name::FORMAT))
            .expect("format span");
        assert_eq!(format.name, "format anthropic");
        assert_eq!(
            format.parent_span_id.as_deref(),
            Some(root.span_id.as_str())
        );
        assert_eq!(format.trace_id, root.trace_id);
    }

    /// Groups its exports so batch boundaries are observable.
    #[derive(Debug, Default)]
    struct GroupingExporter {
        calls: Mutex<Vec<usize>>,
    }

    #[async_trait::async_trait]
    impl TraceExporter for GroupingExporter {
        async fn export(&self, spans: &[Span]) -> crate::error::StudioResult<()> {
            if !spans.is_empty() {
                self.calls.lock().expect("calls lock").push(spans.len());
            }
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn spans_are_exported_on_the_schedule_without_a_flush() {
        // STUDIO-02: a long-lived tracer (`MiddlewareChain` holds it) must not depend on the
        // host calling `flush()`; the `BatchSpanProcessor`-style schedule does it.
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone());
        let stream = tracing.on_agent(
            &ctx(Some("run-schedule")),
            agent_input(),
            next_with(vec![Ok(AgentEvent::new(AgentEventType::AgentStart))]),
        );
        let _ = drain(stream).await;
        assert_eq!(
            exporter.len(),
            0,
            "nothing exports before the schedule fires"
        );

        // No `flush()`: only the tokio clock moves (default interval is 5s). Under
        // `start_paused` the runtime may auto-advance to earlier timers, so step the clock
        // in interval-sized jumps until the schedule fires (at most two jumps are needed).
        let mut exported = false;
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(6)).await;
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            if !exporter.is_empty() {
                exported = true;
                break;
            }
        }
        assert!(
            exported,
            "the batch schedule must export finished spans without an explicit flush"
        );
    }

    #[tokio::test]
    async fn a_full_batch_is_exported_immediately() {
        // STUDIO-02: the batch size caps the buffer, so spans must not wait for the schedule.
        let exporter = Arc::new(GroupingExporter::default());
        let config = StudioConfig::builder()
            .telemetry_batch_interval(Duration::from_secs(3600))
            .telemetry_batch_size(2)
            .build();
        let tracing = TelemetryTracing::with_config(exporter.clone(), &config);

        for run in 0..3 {
            let run_id = format!("run-cap-{run}");
            let stream = tracing.on_agent(
                &ctx(Some(run_id.as_str())),
                agent_input(),
                next_with(vec![Ok(AgentEvent::new(AgentEventType::AgentStart))]),
            );
            let _ = drain(stream).await;
        }

        // The export happens on the sink's background task; give it a few polls (no sleeping).
        for _ in 0..64 {
            if !exporter.calls.lock().expect("calls lock").is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let calls = exporter.calls.lock().expect("calls lock").clone();
        assert_eq!(
            calls.first(),
            Some(&2),
            "a full batch must be exported at once, got {calls:?}"
        );
    }

    #[test]
    fn the_traced_points_cover_the_four_java_calls() {
        // Java `Tracer`: callAgent / callModel / callTool / callFormat. STUDIO-12 closed the
        // last gap by taking the format span off core's `ExtensionPoint::OnFormat` seam.
        let tracing = TelemetryTracing::new(Arc::new(RecordingTraceExporter::new()));
        let points = tracing.active_points();
        assert_eq!(
            points,
            vec![
                ExtensionPoint::OnAgent,
                ExtensionPoint::OnModelCall,
                ExtensionPoint::OnActing,
                ExtensionPoint::OnFormat
            ]
        );
    }

    #[tokio::test]
    async fn the_framework_format_hook_emits_a_format_span() {
        // STUDIO-12: the span comes from the core seam, so a structured-output call is traced
        // without the host having to call `call_format`.
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone()).with_provider_name("anthropic");
        let mut chain = agentscope_core::middleware::MiddlewareChain::new();
        chain.add(Box::new(tracing.clone()));

        let requested = Some(ResponseFormat::json_object());
        let mut format = requested.clone();
        chain
            .apply_format(&ctx(Some("run-format-hook")), &mut format)
            .await;
        assert_eq!(format, requested, "the tracer must not rewrite the request");
        tracing.flush().await;

        let spans = exporter.spans();
        assert_eq!(spans.len(), 1);
        let format = &spans[0];
        assert_eq!(format.name, "format anthropic");
        assert_eq!(format.kind, SpanKind::Internal);
        // Java's `finally { span.end(); }` leaves the status unset when it never sets one.
        assert_eq!(format.status, SpanStatus::Unset);
        assert_eq!(
            attr_str(format, gen_ai::GEN_AI_OPERATION_NAME),
            as_attrs::operation_name::FORMAT
        );
        assert_eq!(
            attr_str(format, as_attrs::AGENTSCOPE_FORMAT_TARGET),
            "anthropic"
        );
        assert_eq!(
            attr_str(format, as_attrs::AGENTSCOPE_FUNCTION_NAME),
            "AnthropicChatFormatter.format"
        );
        // The hook carries no message list: Java's `msgList == null` branch.
        assert!(attr_value(format, as_attrs::AGENTSCOPE_FUNCTION_INPUT).is_none());
    }

    #[tokio::test]
    async fn an_unconstrained_call_emits_no_format_span() {
        // Java only traces `callFormat` when a format is requested.
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone());
        let mut chain = agentscope_core::middleware::MiddlewareChain::new();
        chain.add(Box::new(tracing.clone()));

        let mut format: Option<ResponseFormat> = None;
        chain
            .apply_format(&ctx(Some("run-no-format")), &mut format)
            .await;
        tracing.flush().await;
        assert!(exporter.is_empty());
    }

    /// Answers with the JSON a native structured-output call expects, and reports its provider.
    struct StructuredModel;

    #[async_trait::async_trait]
    impl agentscope_core::model::Model for StructuredModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<agentscope_core::model::ToolSchema>,
            _options: agentscope_core::model::GenerateOptions,
        ) -> futures::stream::BoxStream<
            '_,
            agentscope_core::error::Result<agentscope_core::model::ChatResponse>,
        > {
            use agentscope_core::message::{ContentBlock, TextBlock};
            let response = agentscope_core::model::ChatResponse::builder()
                .id("resp-structured")
                .content(vec![ContentBlock::Text(TextBlock::new(
                    r#"{"answer":"ok"}"#,
                ))])
                .finish_reason("stop")
                .build();
            Box::pin(futures::stream::iter(vec![Ok(response)]))
        }

        fn model_name(&self) -> &str {
            "claude-test"
        }

        fn provider_name(&self) -> &str {
            "anthropic"
        }

        fn supports_native_structured_output(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn a_structured_call_runs_the_format_span_through_the_core_seam() {
        // Real assembly path: the chain the agent was built with drives `on_format`.
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone());
        let mut chain = agentscope_core::middleware::MiddlewareChain::new();
        chain.add(Box::new(tracing.clone()));

        let agent = agentscope_core::agent::ReActAgent::builder()
            .name("traced")
            .model(Arc::new(StructuredModel))
            .middleware_chain(chain)
            .build()
            .expect("agent builds");

        let reply = agent
            .call_with_schema(
                vec![Msg::user("hi")],
                agentscope_core::formatter::JsonSchema::default(),
            )
            .await
            .expect("structured call succeeds");
        assert!(reply.text_content().contains("ok"));
        tracing.flush().await;

        let spans = exporter.spans();
        let root = spans
            .iter()
            .find(|s| s.name == format!("{} traced", operation_name::INVOKE_AGENT))
            .expect("agent span");
        let format = spans
            .iter()
            .find(|s| s.name.starts_with(as_attrs::operation_name::FORMAT))
            .expect("format span");
        assert_eq!(
            format.parent_span_id.as_deref(),
            Some(root.span_id.as_str())
        );
        assert_eq!(format.trace_id, root.trace_id);
        // STUDIO-13: no host configuration needed — the model reports its own provider.
        let chat = spans
            .iter()
            .find(|s| s.name.starts_with(operation_name::CHAT))
            .expect("model span");
        assert_eq!(attr_str(chat, gen_ai::GEN_AI_PROVIDER_NAME), "anthropic");
        assert_eq!(
            attr_str(chat, as_attrs::AGENTSCOPE_FUNCTION_NAME),
            "AnthropicChatModel.callModel"
        );
    }

    /// A model that reports its provider family, like the extension providers do.
    struct ProviderModel;

    #[async_trait::async_trait]
    impl agentscope_core::model::Model for ProviderModel {
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
            "claude-test"
        }

        fn provider_name(&self) -> &str {
            "anthropic"
        }
    }

    /// Runs one model call through the hook and returns its exported span.
    async fn exported_chat_span(
        exporter: &Arc<RecordingTraceExporter>,
        tracing: &TelemetryTracing,
        model: Arc<dyn Model>,
        run: &str,
    ) -> Span {
        let input = ModelCallInput::new(
            vec![Msg::user("hi")],
            Vec::new(),
            agentscope_core::model::GenerateOptions::default(),
            model,
        );
        let next: Next<ModelCallInput> =
            Box::new(|_| -> EventStream { Box::pin(futures::stream::empty()) });
        let _ = drain(tracing.on_model_call(&ctx(Some(run)), input, next)).await;
        tracing.flush().await;
        exporter
            .spans()
            .into_iter()
            .find(|s| s.name.starts_with(operation_name::CHAT))
            .expect("model span")
    }

    #[tokio::test]
    async fn the_model_reported_provider_name_wins_over_the_configured_fallback() {
        // STUDIO-13: `Model::provider_name` is the primary source, so both the provider
        // attribute and the model type in `agentscope.function.name` are right without the
        // host configuring anything.
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone()).with_provider_name("openai");
        let chat =
            exported_chat_span(&exporter, &tracing, Arc::new(ProviderModel), "run-provider").await;
        assert_eq!(attr_str(&chat, gen_ai::GEN_AI_PROVIDER_NAME), "anthropic");
        assert_eq!(
            attr_str(&chat, as_attrs::AGENTSCOPE_FUNCTION_NAME),
            "AnthropicChatModel.callModel"
        );
    }

    #[tokio::test]
    async fn a_model_without_a_provider_name_uses_the_configured_fallback() {
        let exporter = Arc::new(RecordingTraceExporter::new());
        let tracing = TelemetryTracing::new(exporter.clone()).with_provider_name("dashscope");
        let chat =
            exported_chat_span(&exporter, &tracing, Arc::new(StubModel), "run-fallback").await;
        assert_eq!(attr_str(&chat, gen_ai::GEN_AI_PROVIDER_NAME), "dashscope");
        assert_eq!(
            attr_str(&chat, as_attrs::AGENTSCOPE_FUNCTION_NAME),
            "DashScopeChatModel.callModel"
        );
    }

    /// Fails the first `failures` exports, then records: STUDIO-06's retry path.
    #[derive(Debug)]
    struct FlakyExporter {
        failures: usize,
        attempts: Mutex<usize>,
        recorded: Mutex<Vec<Span>>,
    }

    impl FlakyExporter {
        fn new(failures: usize) -> Self {
            Self {
                failures,
                attempts: Mutex::new(0),
                recorded: Mutex::new(Vec::new()),
            }
        }

        fn attempts(&self) -> usize {
            *self.attempts.lock().expect("attempts lock")
        }

        fn recorded(&self) -> Vec<Span> {
            self.recorded.lock().expect("recorded lock").clone()
        }
    }

    #[async_trait::async_trait]
    impl TraceExporter for FlakyExporter {
        async fn export(&self, spans: &[Span]) -> crate::error::StudioResult<()> {
            let attempt = {
                let mut attempts = self.attempts.lock().expect("attempts lock");
                *attempts += 1;
                *attempts
            };
            if attempt <= self.failures {
                return Err(crate::error::StudioError::WebSocket(format!(
                    "export {attempt} failed"
                )));
            }
            self.recorded
                .lock()
                .expect("recorded lock")
                .extend_from_slice(spans);
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_failed_export_is_retried_until_the_spans_arrive() {
        // STUDIO-06: a single failed POST used to drop the whole batch.
        let exporter = Arc::new(FlakyExporter::new(2));
        let tracing = TelemetryTracing::new(exporter.clone());
        let stream = tracing.on_agent(
            &ctx(Some("run-retry")),
            agent_input(),
            next_with(vec![Ok(AgentEvent::new(AgentEventType::AgentStart))]),
        );
        let _ = drain(stream).await;
        // `flush` waits for the export to settle, retries included.
        tracing.flush().await;

        assert_eq!(exporter.attempts(), EXPORT_MAX_ATTEMPTS as usize);
        let recorded = exporter.recorded();
        assert_eq!(
            recorded.len(),
            1,
            "the retried batch must reach the exporter"
        );
        assert_eq!(recorded[0].name, expected_agent_span_name());
    }

    #[tokio::test]
    async fn a_permanently_failing_export_gives_up_without_blocking_the_sink() {
        let exporter = Arc::new(FlakyExporter::new(usize::MAX));
        let tracing = TelemetryTracing::new(exporter.clone());
        let stream = tracing.on_agent(
            &ctx(Some("run-dead-endpoint")),
            agent_input(),
            next_with(vec![Ok(AgentEvent::new(AgentEventType::AgentStart))]),
        );
        let _ = drain(stream).await;
        // Must return: an unreachable endpoint cannot retry forever.
        tracing.flush().await;

        assert_eq!(exporter.attempts(), EXPORT_MAX_ATTEMPTS as usize);
        assert!(exporter.recorded().is_empty());
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
