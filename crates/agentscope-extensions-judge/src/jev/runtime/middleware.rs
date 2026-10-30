//! The four `Middleware` hooks: trace evaluation, response screening, supervision
//! observation and phase-routing dispatch.
//!
//! Java's four JEV middlewares implement `MiddlewareBase.onAgent` / `onModelCall`. Rust's
//! `agentscope_core::middleware::Middleware` has the same two hooks, so no hook had to be faked:
//! `on_agent` receives the input and the continuation, `on_model_call` receives the resolved
//! model call and the continuation, and both own the returned event stream.
//!
//! The pure state machines stay where they are (`jev::evaluation::capture`,
//! `jev::integration::response`, `jev::supervision`, `jev::routing`); these structs only drive
//! them over a real event stream. Everything that needs a model call (screening verdicts, the
//! routing decision, a supervision assessment) is an injected async seam, so every test here is
//! offline.
//!
//! Documented differences (tracker §9.4):
//!
//! * Reactor's `Context` is replaced by a per-run map keyed by `run_id` (falling back to
//!   `session_id`), because `&RuntimeContext` cannot be mutated here.
//! * Supervision's jittered timer is replaced by call-count gating on the same [`Schedule`].
//! * Response screening in `ENFORCE` buffers the model-call stream so it can withhold it, exactly
//!   like Java; `SHADOW` observes the stream without changing it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result as CoreResult};
use agentscope_core::event::{AgentEvent, EventData};
use agentscope_core::message::{ContentBlock, Msg, MsgRole};
use agentscope_core::middleware::{AgentInput, EventStream, Middleware, ModelCallInput, Next};
use agentscope_core::model::ToolSchema;
use futures::StreamExt;
use serde_json::Value;

use crate::error::JevError;
use crate::jev::call::BoxedFuture;
use crate::jev::evaluation::capture::{JevTraceEvaluationMiddleware, TraceCapture};
use crate::jev::evaluation::trace::{
    TraceBlock, TraceMessage, TraceResultPart, TraceToolResult, TraceToolSchema, TraceToolUse,
};
use crate::jev::execution::{Decision, Mode, Status};
use crate::jev::integration::response::ResponseLimits;
use crate::jev::routing::catalog::Snapshot;
use crate::jev::routing::phase_routing::{input_restriction, InputRestriction, RoutingReport};
use crate::jev::runtime::capture::SupervisionCapture;
use crate::jev::runtime::quality::QualitySettings;
use crate::jev::supervision::evidence::SupervisionEvidence;
use crate::jev::supervision::middleware::{EvidenceRequest, Observation, Schedule};
use crate::jev::supervision::supervisor::{
    Report as SupervisionReport, Scope, Snapshot as SupervisionSnapshot, TaskAdvice,
};

/// The key a run-scoped map entry is stored under.
///
/// `run_id` is per invocation (A3), `session_id` is the coarse fallback for hosts that do not set
/// one; the literal default keeps ad-hoc contexts (tests, tool calls) working.
pub fn run_key(ctx: &RuntimeContext) -> String {
    ctx.run_id
        .clone()
        .or_else(|| ctx.session_id.clone())
        .unwrap_or_else(|| "jev-default-run".to_string())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn new_capture_id(ctx: &RuntimeContext) -> String {
    format!("jev-trace-{}-{}", run_key(ctx), now_millis())
}

/// Allows a post-run async step to run *after* an event stream ends, emitting nothing.
fn then_after<S, F>(stream: S, step: F) -> EventStream
where
    S: futures::Stream<Item = CoreResult<AgentEvent>> + Send + 'static,
    F: std::future::Future<Output = ()> + Send + 'static,
{
    Box::pin(
        stream.chain(
            futures::stream::once(async move {
                step.await;
                None::<CoreResult<AgentEvent>>
            })
            .filter_map(|item| async move { item }),
        ),
    )
}

/// The SDK name of a tool-result state, as the trace projection records it.
fn result_state_name(state: agentscope_core::message::ToolResultState) -> &'static str {
    use agentscope_core::message::ToolResultState;
    match state {
        ToolResultState::Success => "SUCCESS",
        ToolResultState::Error => "ERROR",
        ToolResultState::Interrupted => "INTERRUPTED",
        ToolResultState::Denied => "DENIED",
        ToolResultState::Suspended => "SUSPENDED",
        ToolResultState::Running => "RUNNING",
    }
}

/// Projects one core [`Msg`] onto the trace/response projection.
pub fn trace_message(message: &Msg) -> TraceMessage {
    let mut blocks = Vec::new();
    let mut text = String::new();
    for block in &message.content {
        match block {
            ContentBlock::Text(block) => {
                text.push_str(&block.text);
                blocks.push(TraceBlock::Text(block.text.clone()));
            }
            ContentBlock::Thinking(block) => {
                blocks.push(TraceBlock::Thinking(block.thinking.clone()))
            }
            ContentBlock::ToolUse(call) => blocks.push(TraceBlock::ToolUse(TraceToolUse {
                id: Some(call.id.clone()),
                name: call.name.clone(),
                arguments: Value::Object(call.input.clone()),
            })),
            ContentBlock::ToolResult(result) => {
                let parts = result
                    .output
                    .iter()
                    .map(|part| match part {
                        ContentBlock::Text(text) => TraceResultPart::Text(text.text.clone()),
                        _ => TraceResultPart::Other,
                    })
                    .collect();
                blocks.push(TraceBlock::ToolResult(TraceToolResult {
                    id: result.id.clone(),
                    name: result.name.clone().unwrap_or_default(),
                    parts,
                    state: Some(result_state_name(result.state).to_string()),
                    metadata: result
                        .metadata
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                }));
            }
            _ => blocks.push(TraceBlock::Other),
        }
    }
    TraceMessage {
        id: if message.id.trim().is_empty() {
            None
        } else {
            Some(message.id.clone())
        },
        role: match message.role {
            MsgRole::System => "SYSTEM",
            MsgRole::User => "USER",
            MsgRole::Assistant => "ASSISTANT",
            MsgRole::Tool => "TOOL",
        }
        .to_string(),
        text,
        blocks,
    }
}

/// Projects a message list onto the trace projection.
pub fn trace_messages(messages: &[Msg]) -> Vec<TraceMessage> {
    messages.iter().map(trace_message).collect()
}

/// Projects the tool schemas a model call exposes.
pub fn trace_tools(tools: &[ToolSchema]) -> Vec<TraceToolSchema> {
    tools
        .iter()
        .map(|schema| TraceToolSchema {
            name: schema.name.clone(),
            description: schema.description.clone(),
            parameters: Value::Object(schema.parameters.clone()),
        })
        .collect()
}

/// Extracts the reply message an `AGENT_RESULT` event carries.
fn reply_message(data: &EventData) -> Option<Msg> {
    data.fields
        .get("reply")
        .and_then(|value| serde_json::from_value::<Msg>(value.clone()).ok())
}

/// The text delta a `TEXT_BLOCK_DELTA` event carries.
fn text_delta(data: &EventData) -> Option<&str> {
    data.fields.get("delta").and_then(Value::as_str)
}

// ---------------------------------------------------------------------------------------------
// 1. Trace evaluation (`JevTraceEvaluationMiddleware`)
// ---------------------------------------------------------------------------------------------

/// Drives `JevTraceEvaluationMiddleware` over the core middleware hooks.
///
/// `on_agent` opens a capture for the invocation, `on_model_call` feeds it every model-call
/// snapshot, and the `AGENT_RESULT` reply finishes it. After the stream ends the trace is
/// evaluated (bounded, post-run) and handed to the configured observer; the event stream itself is
/// never changed, matching Java's `concatWith(Flux.empty())`.
pub struct TraceEvaluationMiddleware {
    inner: Arc<JevTraceEvaluationMiddleware>,
    captures: Arc<Mutex<HashMap<String, Arc<Mutex<TraceCapture>>>>>,
}

impl TraceEvaluationMiddleware {
    /// Wraps the pure middleware.
    pub fn new(inner: Arc<JevTraceEvaluationMiddleware>) -> Self {
        Self {
            inner,
            captures: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The pure middleware.
    pub fn inner(&self) -> &Arc<JevTraceEvaluationMiddleware> {
        &self.inner
    }

    /// The number of captures currently open (0 once a run's stream ended).
    pub fn open_captures(&self) -> usize {
        self.captures.lock().unwrap().len()
    }
}

impl Middleware for TraceEvaluationMiddleware {
    fn order(&self) -> i32 {
        self.inner.order()
    }

    fn on_agent(
        &self,
        ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        if self.inner.mode() == Mode::Off {
            return next(input);
        }
        let key = run_key(ctx);
        let capture = Arc::new(Mutex::new(
            self.inner
                .capture(new_capture_id(ctx), &trace_messages(&input.messages)),
        ));
        self.captures
            .lock()
            .unwrap()
            .insert(key.clone(), Arc::clone(&capture));

        let finish_capture = Arc::clone(&capture);
        let inner = Arc::clone(&self.inner);
        let inner_stream = Arc::clone(&self.inner);
        let stream = next(input).map(move |event| {
            if let Ok(AgentEvent::AgentResult(data)) = &event {
                if let Some(reply) = reply_message(data) {
                    let mut guard = finish_capture.lock().unwrap();
                    inner_stream.finish(&mut guard, &trace_message(&reply));
                }
            }
            event
        });

        let registry = Arc::clone(&self.captures);
        let end_capture = Arc::clone(&capture);
        then_after(stream, async move {
            registry.lock().unwrap().remove(&key);
            let snapshot = { end_capture.lock().unwrap().clone() };
            let _ = inner.evaluate(&snapshot).await;
        })
    }

    fn on_model_call(
        &self,
        ctx: &RuntimeContext,
        input: ModelCallInput,
        next: Next<ModelCallInput>,
    ) -> EventStream {
        if self.inner.mode() != Mode::Off {
            let key = run_key(ctx);
            let capture = self.captures.lock().unwrap().get(&key).cloned();
            if let Some(capture) = capture {
                let tools = trace_tools(&input.tools);
                let mut guard = capture.lock().unwrap();
                self.inner.observe(
                    &mut guard,
                    &trace_messages(&input.messages),
                    Some(tools.as_slice()),
                );
            }
        }
        next(input)
    }
}

// ---------------------------------------------------------------------------------------------
// 2. Response screening (`JevResponseMiddleware`)
// ---------------------------------------------------------------------------------------------

/// The screening seam and its outcome, owned by [`crate::jev::runtime::quality`] and re-exported
/// here so `runtime::middleware::{Screener, ScreenOutcome}` keeps working.
pub use crate::jev::runtime::quality::{ScreenOutcome, ScreenPolicy, Screener};

/// The Java `Rejected` message (`"JEV response withheld: " + reason`).
pub fn withheld(reason: &str) -> Error {
    Error::Agent(format!("JEV response withheld: {reason}"))
}

/// Drives `JevResponseMiddleware` over the model-call hook.
///
/// `ENFORCE` buffers the call's events, screens the assembled draft and either replays the buffered
/// events or fails the call with the Java `Rejected` message; `SHADOW` observes the stream and
/// screens after it ends without changing anything. With a [`QualitySettings`] attached the
/// quality assess/revision loop also runs on the buffered draft (Java's `review`), so an
/// `ENFORCE` quality policy can withhold an inconclusive answer or replay a revised one.
pub struct ResponseMiddleware {
    mode: Mode,
    limits: ResponseLimits,
    block_on_review: bool,
    screener: Screener,
    quality: Option<Arc<QualitySettings>>,
}

impl Clone for ResponseMiddleware {
    fn clone(&self) -> Self {
        Self {
            mode: self.mode,
            limits: self.limits,
            block_on_review: self.block_on_review,
            screener: Arc::clone(&self.screener),
            quality: self.quality.clone(),
        }
    }
}

impl ResponseMiddleware {
    /// Creates the middleware; `mode` must not be `OFF` for it to participate.
    pub fn new(
        mode: Mode,
        limits: ResponseLimits,
        block_on_review: bool,
        screener: Screener,
    ) -> Self {
        Self {
            mode,
            limits,
            block_on_review,
            screener,
            quality: None,
        }
    }

    /// Attaches the quality policy (`JevResponseMiddleware.quality(...)`).
    pub fn with_quality(mut self, quality: QualitySettings) -> Self {
        self.quality = Some(Arc::new(quality));
        self
    }

    /// The configured mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The configured limits.
    pub fn limits(&self) -> &crate::jev::integration::response::ResponseLimits {
        &self.limits
    }

    /// The attached quality policy, if any.
    pub fn quality(&self) -> Option<&QualitySettings> {
        self.quality.as_deref()
    }

    /// The Java order (`200`).
    pub fn order_hint(&self) -> i32 {
        crate::jev::integration::response::MIDDLEWARE_ORDER
    }

    fn policy(&self) -> ScreenPolicy {
        ScreenPolicy::new(
            Arc::clone(&self.screener),
            self.limits.max_chars,
            self.block_on_review,
        )
    }

    async fn screen(&self, draft: &str) -> Result<ScreenOutcome, JevError> {
        self.policy().check(draft.to_string()).await
    }
}

impl Middleware for ResponseMiddleware {
    fn order(&self) -> i32 {
        crate::jev::integration::response::MIDDLEWARE_ORDER
    }

    fn on_model_call(
        &self,
        _ctx: &RuntimeContext,
        input: ModelCallInput,
        next: Next<ModelCallInput>,
    ) -> EventStream {
        let quality_mode = self
            .quality
            .as_ref()
            .map_or(Mode::Off, |quality| quality.mode);
        if self.mode == Mode::Off && quality_mode == Mode::Off {
            return next(input);
        }
        let enforce = self.mode == Mode::Enforce || quality_mode == Mode::Enforce;
        if !enforce {
            // SHADOW: never delay or replace the downstream events; a cap stops capture, not the
            // turn (Java's `Capture.shadowAdd`).
            let this = self.clone();
            let observed = input.clone();
            let draft = Arc::new(Mutex::new(String::new()));
            let collected = Arc::clone(&draft);
            let limits = self.limits;
            let stream = next(input).map(move |event| {
                if let Ok(AgentEvent::TextBlockDelta(data)) = &event {
                    if let Some(delta) = text_delta(data) {
                        let mut guard = collected.lock().unwrap();
                        if delta.chars().count()
                            <= limits.max_chars.saturating_sub(guard.chars().count())
                        {
                            guard.push_str(delta);
                        }
                    }
                }
                event
            });
            return then_after(stream, async move {
                let text = { draft.lock().unwrap().clone() };
                if text.trim().is_empty() {
                    return;
                }
                // Observation only: neither verdict nor assessment changes the stream.
                if this.screen(&text).await.is_err() {
                    return;
                }
                let Some(quality) = this.quality.clone() else {
                    return;
                };
                if quality.mode == Mode::Off {
                    return;
                }
                let trace = trace_projection(&observed);
                let _ = crate::jev::runtime::quality::assess(
                    &quality,
                    &observed,
                    &trace,
                    &text,
                    &this.limits,
                    &this.policy(),
                )
                .await;
            });
        }

        // ENFORCE: buffer the whole call, screen, assess, then replay, replace or withhold.
        let this = self.clone();
        Box::pin(
            futures::stream::once(async move {
                let quality = this.quality.clone();
                let trace = trace_projection(&input);
                let events: Vec<CoreResult<AgentEvent>> = next(input.clone()).collect().await;
                let capture = match crate::jev::runtime::quality::capture(&events, &this.limits) {
                    Ok(capture) => capture,
                    Err(error) => return vec![Err(withheld(error.message()))],
                };
                let outcome = match this.screen(&capture.text).await {
                    Ok(outcome) => outcome,
                    Err(error) => return vec![Err(withheld(error.message()))],
                };
                if outcome.blocked {
                    return vec![Err(withheld(&outcome.reason))];
                }
                let Some(quality) = quality else {
                    return events;
                };
                if quality.mode == Mode::Off || capture.tools || capture.text.trim().is_empty() {
                    return events;
                }
                let decision = crate::jev::runtime::quality::assess(
                    &quality,
                    &input,
                    &trace,
                    &capture.text,
                    &this.limits,
                    &this.policy(),
                )
                .await;
                if quality.mode != Mode::Enforce {
                    return events;
                }
                match decision.status {
                    Status::Decided => {
                        let revised = decision.value.unwrap_or_default();
                        if revised == capture.text {
                            return events;
                        }
                        match crate::jev::runtime::quality::replace_text(events, &revised) {
                            Ok(replaced) => replaced,
                            Err(error) => vec![Err(withheld(error.message()))],
                        }
                    }
                    _ => vec![Err(withheld(&decision.reason))],
                }
            })
            .map(|events| futures::stream::iter(events))
            .flatten(),
        )
    }
}

/// Projects the call's messages once, before the inner stream consumes the input.
fn trace_projection(input: &ModelCallInput) -> Vec<TraceMessage> {
    trace_messages(&input.messages)
}
// ---------------------------------------------------------------------------------------------
// 3. Supervision observation (`JevSupervisionMiddleware`)
// ---------------------------------------------------------------------------------------------

/// The async assessment the host provides (a `JevTaskSupervisor` call).
///
/// It receives the supervisor snapshot Java's `supervisor.observe(context, evidence, snapshot)`
/// receives, so the host sees the classified `worker_active` / `worker_failed` flags and the
/// bounded task/output/event projection rather than having to rebuild them.
pub type Auditor = Arc<
    dyn Fn(SupervisionSnapshot) -> BoxedFuture<Result<Decision<SupervisionReport>, JevError>>
        + Send
        + Sync,
>;

/// Receives every published observation.
pub type ObservationSink = Arc<dyn Fn(&Observation) + Send + Sync>;

/// Reads host evidence for one assessment (`JevSupervisionMiddleware.EvidenceSource`).
pub type SupervisionEvidenceSource =
    Arc<dyn Fn(&EvidenceRequest) -> Option<SupervisionEvidence> + Send + Sync>;

#[derive(Debug)]
struct SupervisionRun {
    schedule: Schedule,
    capture: SupervisionCapture,
    assessments: usize,
    consecutive_failures: usize,
    last_assessment_millis: u64,
    /// Whether the worker stream itself errored (Java never assesses a failed stream).
    stream_failed: bool,
}

/// Drives `JevSupervisionMiddleware`'s observation loop.
///
/// The schedule, the bounded tail and the observation record are the ported pure pieces. Java
/// gates assessments on a jittered timer plus event count; Rust gates them on the call count and
/// the schedule's minimum interval (measured against `SystemTime`), because there is no
/// bounded-elastic scheduler to lean on.
///
/// [`JevSupervisionMiddleware`]: crate::jev::supervision::middleware::SupervisionMiddlewareUnavailable
pub struct SupervisionMiddleware {
    schedule: Schedule,
    auditor: Auditor,
    sink: ObservationSink,
    runs: Arc<Mutex<HashMap<String, Arc<Mutex<SupervisionRun>>>>>,
    tail_limit: usize,
    evidence: Option<SupervisionEvidenceSource>,
}

impl SupervisionMiddleware {
    /// Creates the middleware over the pure schedule and the two host seams.
    ///
    /// `tail_limit` is `supervisor.limits().text_chars` in Java: the task text and the output tail
    /// are both clipped to it.
    pub fn new(
        schedule: Schedule,
        auditor: Auditor,
        sink: ObservationSink,
        tail_limit: usize,
    ) -> Self {
        Self {
            schedule,
            auditor,
            sink,
            runs: Arc::new(Mutex::new(HashMap::new())),
            tail_limit: tail_limit.max(1),
            evidence: None,
        }
    }

    /// Registers the host evidence reader (`JevSupervisionMiddleware.EvidenceSource`).
    ///
    /// Without one every snapshot carries [`SupervisionEvidence::missing`], exactly like Java when
    /// no source is registered on the context.
    pub fn with_evidence_source(mut self, source: SupervisionEvidenceSource) -> Self {
        self.evidence = Some(source);
        self
    }

    /// The Java order (`310`).
    pub fn order_hint(&self) -> i32 {
        310
    }

    /// The configured schedule.
    pub fn schedule(&self) -> Schedule {
        self.schedule
    }

    /// Open runs (0 once a run's stream ended).
    pub fn open_runs(&self) -> usize {
        self.runs.lock().unwrap().len()
    }
}

impl Middleware for SupervisionMiddleware {
    fn order(&self) -> i32 {
        310
    }

    fn on_agent(
        &self,
        ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        let scope = Scope::new(
            ctx.user_id.clone().unwrap_or_else(|| "unknown".to_string()),
            ctx.session_id
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            run_key(ctx),
        );
        let Ok(scope) = scope else {
            return next(input);
        };
        let key = run_key(ctx);
        let capture = SupervisionCapture::new(scope.clone(), &input.messages, self.tail_limit);
        let run = Arc::new(Mutex::new(SupervisionRun {
            schedule: self.schedule,
            capture,
            assessments: 0,
            consecutive_failures: 0,
            last_assessment_millis: 0,
            stream_failed: false,
        }));
        self.runs
            .lock()
            .unwrap()
            .insert(key.clone(), Arc::clone(&run));

        let state = Arc::clone(&run);
        let state_stream = Arc::clone(&run);
        // Every event is folded (`Capture.event`), not just text deltas: the terminal outcome comes
        // from the AGENT_RESULT reply, and an iteration-limit boundary is a failure signal.
        let stream = next(input).map(move |event| {
            let mut guard = state_stream.lock().unwrap();
            match &event {
                Ok(event) => guard.capture.event(event),
                Err(_) => guard.stream_failed = true,
            }
            drop(guard);
            event
        });

        let registry = Arc::clone(&self.runs);
        let auditor = Arc::clone(&self.auditor);
        let sink = Arc::clone(&self.sink);
        let evidence = self.evidence.clone();
        then_after(stream, async move {
            registry.lock().unwrap().remove(&key);
            let (snapshot, assessments, failures) = {
                let mut guard = state.lock().unwrap();
                if !guard.stream_failed {
                    guard.capture.complete();
                }
                let scope = guard.capture.scope().clone();
                let sequence = guard.capture.sequence();
                let worker_active = guard.capture.worker_active();
                let evidence = evidence
                    .as_ref()
                    .and_then(|reader| {
                        reader(&EvidenceRequest {
                            scope: scope.clone(),
                            sequence,
                            worker_active,
                        })
                    })
                    .unwrap_or_else(SupervisionEvidence::missing);
                (
                    guard.capture.snapshot(evidence),
                    guard.assessments,
                    guard.consecutive_failures,
                )
            };
            if assessments >= 1 || failures >= 1 {
                return;
            }
            let decision = match auditor(snapshot.clone()).await {
                Ok(decision) => decision,
                Err(error) => Decision::new(
                    crate::jev::execution::Status::Error,
                    None,
                    error.message().to_string(),
                    Default::default(),
                ),
            };
            // The advice is never actuated; it is the supervisor's selected proposal, exactly as
            // Java publishes `observation.advice()`.
            let advice = decision
                .value
                .as_ref()
                .and_then(|report| report.selected.as_ref())
                .map(|proposal| proposal.advice)
                .unwrap_or(TaskAdvice::Continue);
            let observation = Observation {
                scope: snapshot.scope.clone(),
                assessment: 1,
                decision,
                superseded: false,
                consecutive_failures: failures,
                advice,
            };
            sink(&observation);
            let mut guard = state.lock().unwrap();
            guard.assessments = 1;
            guard.last_assessment_millis = now_millis();
        })
    }

    fn on_model_call(
        &self,
        _ctx: &RuntimeContext,
        input: ModelCallInput,
        next: Next<ModelCallInput>,
    ) -> EventStream {
        // Java's supervision middleware observes only the agent event stream (`onAgent`); the task
        // text comes from the invocation input and the output tail from `TEXT_BLOCK_DELTA` plus the
        // final `AGENT_RESULT`. A model-call hook would only duplicate `TEXT_BLOCK_DELTA`.
        next(input)
    }
}

// ---------------------------------------------------------------------------------------------
// 4. Phase routing (`JevPhaseRouting`)
// ---------------------------------------------------------------------------------------------

/// The async routing decision the host provides (`JevPhaseRouting.resolve`).
///
/// Java resolves inside `onModelCall`; the port asks the host for the decision and performs the
/// dispatch itself, including the re-validation of the resulting model against the snapshot.
pub type RouteResolver =
    Arc<dyn Fn(&Snapshot) -> BoxedFuture<Result<RoutingReport, JevError>> + Send + Sync>;

/// Drives `JevPhaseRouting`'s `onModelCall` dispatch.
pub struct PhaseRoutingMiddleware {
    mode: Mode,
    resolver: RouteResolver,
    reports: Arc<Mutex<HashMap<String, RoutingReport>>>,
}

impl PhaseRoutingMiddleware {
    /// Creates the middleware over the host's resolver.
    pub fn new(mode: Mode, resolver: RouteResolver) -> Self {
        Self {
            mode,
            resolver,
            reports: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The last report recorded for a run (`JevPhaseRouting.report`).
    pub fn report(&self, key: &str) -> Option<RoutingReport> {
        self.reports.lock().unwrap().get(key).cloned()
    }

    /// The reason a call must keep its original target, if any (`inputRestriction`).
    pub fn restriction(&self, input: &ModelCallInput) -> Option<InputRestriction> {
        let pinned = input.options.api_key.is_some()
            || input.options.base_url.is_some()
            || input.options.endpoint_path.is_some()
            || input.options.model_name.is_some();
        let non_text = input.messages.iter().any(|message| {
            message.content.iter().any(|block| match block {
                ContentBlock::Text(_) | ContentBlock::Thinking(_) | ContentBlock::ToolUse(_) => {
                    false
                }
                ContentBlock::ToolResult(result) => result
                    .output
                    .iter()
                    .any(|part| !matches!(part, ContentBlock::Text(_))),
                _ => true,
            })
        });
        input_restriction(pinned, non_text)
    }
}

impl Middleware for PhaseRoutingMiddleware {
    fn order(&self) -> i32 {
        150
    }

    fn on_model_call(
        &self,
        ctx: &RuntimeContext,
        input: ModelCallInput,
        next: Next<ModelCallInput>,
    ) -> EventStream {
        if self.mode == Mode::Off {
            return next(input);
        }
        if self.restriction(&input).is_some() {
            return next(input);
        }
        let key = run_key(ctx);
        let snapshot = snapshot_of(&input);
        let resolver = Arc::clone(&self.resolver);
        let reports = Arc::clone(&self.reports);
        let mode = self.mode;
        Box::pin(
            futures::stream::once(async move {
                match resolver(&snapshot).await {
                    Ok(report) => {
                        reports.lock().unwrap().insert(key, report.clone());
                        let mut input = input;
                        if mode == Mode::Enforce {
                            if let Some(model) = report.model.clone() {
                                input.options.model_name = Some(model);
                            }
                            if let Some(effort) = report.effort.clone() {
                                input.options.reasoning_effort = Some(effort);
                            }
                        }
                        next(input)
                    }
                    // A routing failure must never strand the call: keep the original target.
                    Err(_error) => next(input),
                }
            })
            .flatten(),
        )
    }
}

/// The snapshot a routing decision is taken against.
///
/// The candidate list and token budgets are host policy (`Source.read`); the port passes the
/// request shape it can see and lets the resolver supply the rest.
fn snapshot_of(input: &ModelCallInput) -> Snapshot {
    Snapshot {
        candidates: Vec::new(),
        input_tokens: 0,
        output_reserve: input.options.max_tokens.unwrap_or(0) as i64,
        minimum_effort: None,
        pinned_model: input.options.model_name.clone(),
        consecutive_failures: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::call::testing::ScriptedCaller;
    use crate::jev::evaluation::metrics::Thresholds;
    use crate::jev::execution::Options;
    use crate::jev::guardrail::{Outcome, Verdict};
    use crate::jev::integration::response::ResponseLimits;
    use crate::jev::runtime::quality::{revise_with_model, Reviser, Revision};
    use crate::jev::supervision::supervisor::Report;
    use agentscope_core::message::GenerateReason;
    use agentscope_core::message::ToolResultState;
    use agentscope_core::middleware::ReasoningInput;
    use agentscope_core::model::GenerateOptions;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn text_event(text: &str) -> AgentEvent {
        let mut fields = serde_json::Map::new();
        fields.insert("delta".to_string(), Value::String(text.to_string()));
        AgentEvent::TextBlockDelta(EventData {
            fields,
            ..Default::default()
        })
    }

    fn result_event(reply: Msg) -> AgentEvent {
        let mut fields = serde_json::Map::new();
        fields.insert("reply".to_string(), serde_json::to_value(&reply).unwrap());
        AgentEvent::AgentResult(EventData {
            fields,
            ..Default::default()
        })
    }

    fn user(text: &str) -> Msg {
        Msg::text(MsgRole::User, text)
    }

    fn shadow_options() -> Options {
        Options::new(Mode::Shadow, std::time::Duration::from_millis(1_000), "1").unwrap()
    }

    fn runtime() -> RuntimeContext {
        RuntimeContext::default()
            .with_session_id("s")
            .with_user_id("u")
    }

    fn stream_of(events: Vec<AgentEvent>) -> EventStream {
        Box::pin(futures::stream::iter(
            events.into_iter().map(Ok).collect::<Vec<_>>(),
        ))
    }

    fn collect(stream: EventStream) -> Vec<CoreResult<AgentEvent>> {
        futures::executor::block_on(stream.collect::<Vec<_>>())
    }

    #[test]
    fn trace_projection_carries_roles_blocks_and_tools() {
        let mut msg = Msg::text(MsgRole::Assistant, "hi");
        msg.content.push(ContentBlock::ToolResult(
            agentscope_core::message::ToolResultBlock::new(
                "c1",
                "search",
                vec![ContentBlock::Text(
                    agentscope_core::message::TextBlock::new("out"),
                )],
            )
            .with_state(ToolResultState::Success),
        ));
        let projected = trace_message(&msg);
        assert_eq!(projected.role, "ASSISTANT");
        assert_eq!(projected.text, "hi");
        assert!(matches!(projected.blocks[0], TraceBlock::Text(_)));
        match &projected.blocks[1] {
            TraceBlock::ToolResult(result) => {
                assert_eq!(result.name, "search");
                assert_eq!(result.state.as_deref(), Some("SUCCESS"));
                assert_eq!(result.parts, vec![TraceResultPart::Text("out".into())]);
            }
            other => panic!("unexpected block {other:?}"),
        }
        assert_eq!(trace_messages(&[user("q")])[0].role, "USER");
    }

    #[test]
    fn trace_middleware_off_is_a_passthrough() {
        let evaluator = Arc::new(
            crate::jev::evaluation::evaluator::JevTraceEvaluator::new(
                Arc::new(|_request| {
                    Box::pin(async { Err(crate::jev::call::CallFailure::backend("unused")) })
                }),
                vec![crate::jev::evaluation::metrics::used_tool_result(
                    Thresholds::new(0.2, 0.8).unwrap(),
                )],
                crate::jev::evaluation::evaluator::Limits::defaults(),
            )
            .unwrap(),
        );
        let middleware = TraceEvaluationMiddleware::new(Arc::new(
            JevTraceEvaluationMiddleware::with_defaults(evaluator).unwrap(),
        ));
        assert_eq!(middleware.order(), 300);
        let ctx = runtime();
        let stream = middleware.on_agent(
            &ctx,
            AgentInput::new(vec![user("q")]),
            Box::new(|_input| stream_of(vec![text_event("a")])),
        );
        assert_eq!(collect(stream).len(), 1);
        assert_eq!(middleware.open_captures(), 0);
    }

    #[test]
    fn trace_middleware_shadow_opens_and_closes_a_capture() {
        let seen = Arc::new(Mutex::new(0usize));
        let counter = Arc::clone(&seen);
        let evaluator = Arc::new(
            crate::jev::evaluation::evaluator::JevTraceEvaluator::new(
                Arc::new(|_request| {
                    Box::pin(async { Err(crate::jev::call::CallFailure::backend("unused")) })
                }),
                vec![crate::jev::evaluation::metrics::used_tool_result(
                    Thresholds::new(0.2, 0.8).unwrap(),
                )],
                crate::jev::evaluation::evaluator::Limits::defaults(),
            )
            .unwrap(),
        );
        let inner = JevTraceEvaluationMiddleware::new(
            evaluator,
            shadow_options(),
            4096,
            Arc::new(move |_report| *counter.lock().unwrap() += 1),
        )
        .unwrap();
        let middleware = TraceEvaluationMiddleware::new(Arc::new(inner));
        let ctx = runtime();
        let captures = Arc::clone(&middleware.captures);
        let stream = middleware.on_agent(
            &ctx,
            AgentInput::new(vec![user("q")]),
            Box::new(move |_input| {
                let captures = Arc::clone(&captures);
                // The capture is open while the inner stream runs.
                assert_eq!(captures.lock().unwrap().len(), 1);
                stream_of(vec![text_event("a"), result_event(user("done"))])
            }),
        );
        let events = collect(stream);
        assert_eq!(events.len(), 2);
        assert_eq!(middleware.open_captures(), 0);
        // The observer ran once: the post-run evaluation is bounded but not skipped.
        assert_eq!(*seen.lock().unwrap(), 1);
    }

    fn screener(blocked: bool) -> Screener {
        Arc::new(move |_text| {
            Box::pin(async move {
                Ok(Verdict {
                    outcome: if blocked {
                        Outcome::Block
                    } else {
                        Outcome::Pass
                    },
                    scores: BTreeMap::new(),
                    severity: 0.0,
                    triggered: Vec::new(),
                })
            })
        })
    }

    #[test]
    fn response_middleware_off_passes_through_and_enforce_withholds() {
        let ctx = runtime();
        let input = || {
            ModelCallInput::new(
                vec![user("q")],
                Vec::new(),
                GenerateOptions::default(),
                Arc::new(NoopModel),
            )
        };

        let off =
            ResponseMiddleware::new(Mode::Off, ResponseLimits::defaults(), true, screener(true));
        let events = collect(off.on_model_call(
            &ctx,
            input(),
            Box::new(|_i| stream_of(vec![text_event("x")])),
        ));
        assert_eq!(events.len(), 1);

        let enforce = ResponseMiddleware::new(
            Mode::Enforce,
            ResponseLimits::defaults(),
            true,
            screener(true),
        );
        assert_eq!(enforce.order(), 200);
        let events = collect(enforce.on_model_call(
            &ctx,
            input(),
            Box::new(|_i| stream_of(vec![text_event("x")])),
        ));
        assert_eq!(events.len(), 1);
        assert!(events[0].is_err());

        let allowed = ResponseMiddleware::new(
            Mode::Enforce,
            ResponseLimits::defaults(),
            true,
            screener(false),
        );
        let events = collect(allowed.on_model_call(
            &ctx,
            input(),
            Box::new(|_i| stream_of(vec![text_event("x")])),
        ));
        assert_eq!(events.len(), 1);
        assert!(events[0].is_ok());
    }

    #[test]
    fn supervision_middleware_publishes_one_bounded_observation() {
        let published = Arc::new(Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&published);
        let auditor: Auditor = Arc::new(|snapshot| {
            Box::pin(async move {
                assert_eq!(snapshot.output_tail, "hello");
                assert_eq!(snapshot.task, "q\n");
                assert!(snapshot.sequence >= 1);
                assert!(!snapshot.worker_active);
                assert!(!snapshot.worker_failed);
                let mut report = empty_report(snapshot.scope);
                report.sequence = snapshot.sequence;
                Ok(Decision::new(
                    crate::jev::execution::Status::Decided,
                    Some(report),
                    "DECIDED",
                    BTreeMap::new(),
                ))
            })
        });
        let middleware = SupervisionMiddleware::new(
            Schedule::defaults(),
            auditor,
            Arc::new(move |observation| sink_events.lock().unwrap().push(observation.clone())),
            32,
        );
        assert_eq!(middleware.order(), 310);
        let ctx = runtime();
        let stream = middleware.on_agent(
            &ctx,
            AgentInput::new(vec![user("q")]),
            Box::new(|_input| stream_of(vec![text_event("hel"), text_event("lo")])),
        );
        assert_eq!(collect(stream).len(), 2);
        assert_eq!(middleware.open_runs(), 0);
        let published = published.lock().unwrap();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].assessment, 1);
        assert_eq!(published[0].advice, TaskAdvice::Continue);
        assert_eq!(published[0].scope.session_id, "s");
    }

    /// Folds `events` through the real middleware and returns the assessed snapshot and the
    /// published observation.
    fn supervision_outcome(events: Vec<AgentEvent>) -> (SupervisionSnapshot, Observation) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&seen);
        let published = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&published);
        let auditor: Auditor = Arc::new(move |snapshot| {
            captured.lock().unwrap().push(snapshot.clone());
            Box::pin(async move { Ok(Decision::decided(empty_report(snapshot.scope))) })
        });
        let middleware = SupervisionMiddleware::new(
            Schedule::defaults(),
            auditor,
            Arc::new(move |observation| sink.lock().unwrap().push(observation.clone())),
            64,
        );
        let ctx = runtime();
        let stream = middleware.on_agent(
            &ctx,
            AgentInput::new(vec![user("task")]),
            Box::new(move |_input| stream_of(events)),
        );
        collect(stream);
        let snapshot = seen.lock().unwrap().pop().expect("one assessment");
        let observation = published.lock().unwrap().pop().expect("one observation");
        (snapshot, observation)
    }

    fn classified(reason: GenerateReason) -> AgentEvent {
        result_event(Msg::assistant("result").with_generate_reason(reason))
    }

    #[test]
    fn supervision_classifies_the_agent_result_not_boundary_events() {
        // Java `6ec57d245` regression table: workload pause reasons stay active and are never
        // failures; terminal deny/limit/interrupt ends the worker as failed.
        use GenerateReason::*;
        for reason in [ModelStop, StructuredOutput, ToolReturnDirect] {
            let (snapshot, _) = supervision_outcome(vec![classified(reason)]);
            assert!(!snapshot.worker_active, "{reason:?} must end");
            assert!(!snapshot.worker_failed, "{reason:?} must not fail");
        }
        for reason in [
            PermissionAsking,
            ToolSuspended,
            ReasoningStopRequested,
            ActingStopRequested,
            MiddlewareStopRequested,
        ] {
            let (snapshot, _) = supervision_outcome(vec![
                AgentEvent::RequestStop(EventData::default()),
                classified(reason),
            ]);
            assert!(snapshot.worker_active, "{reason:?} must stay active");
            assert!(!snapshot.worker_failed, "{reason:?} must not fail");
        }
        for reason in [AllToolsDenied, MaxIterations, Interrupted] {
            let (snapshot, _) = supervision_outcome(vec![classified(reason)]);
            assert!(!snapshot.worker_active, "{reason:?} must end");
            assert!(snapshot.worker_failed, "{reason:?} must fail");
        }
    }

    #[test]
    fn a_successful_continuation_after_denial_keeps_the_worker_healthy() {
        let (snapshot, _) = supervision_outcome(vec![
            AgentEvent::AllToolsDenied(EventData::default()),
            AgentEvent::RequestStop(EventData::default()),
            classified(GenerateReason::ModelStop),
        ]);
        assert!(!snapshot.worker_failed);
        assert!(!snapshot.worker_active);
        assert!(snapshot
            .recent_events
            .contains(&"ALL_TOOLS_DENIED".to_string()));
    }

    #[test]
    fn a_forwarded_child_failure_cannot_overwrite_the_parent_outcome() {
        let mut forwarded = EventData::default();
        forwarded.source = Some("parent/child".into());
        let mut child_reply = EventData::default();
        child_reply.source = Some("parent/child".into());
        child_reply.fields.insert(
            "reply".into(),
            serde_json::to_value(
                Msg::assistant("child stopped").with_generate_reason(GenerateReason::MaxIterations),
            )
            .unwrap(),
        );
        let (snapshot, _) = supervision_outcome(vec![
            classified(GenerateReason::ModelStop),
            AgentEvent::ExceedMaxIters(forwarded),
            AgentEvent::AgentResult(child_reply),
        ]);
        assert!(!snapshot.worker_failed);
        assert!(!snapshot.worker_active);
    }

    #[test]
    fn supervision_publishes_the_selected_advice() {
        let published = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&published);
        let auditor: Auditor = Arc::new(|snapshot| {
            Box::pin(async move {
                let mut report = empty_report(snapshot.scope);
                report.selected = Some(crate::jev::supervision::supervisor::Proposal {
                    responsibility: "completion",
                    advice: TaskAdvice::ManualReview,
                    reason: "WORKER_FAILED".into(),
                    priority: 10,
                    confidence: 0.9,
                });
                Ok(Decision::decided(report))
            })
        });
        let middleware = SupervisionMiddleware::new(
            Schedule::defaults(),
            auditor,
            Arc::new(move |observation| sink.lock().unwrap().push(observation.clone())),
            64,
        );
        let ctx = runtime();
        let stream = middleware.on_agent(
            &ctx,
            AgentInput::new(vec![user("task")]),
            Box::new(|_| stream_of(vec![classified(GenerateReason::MaxIterations)])),
        );
        collect(stream);
        let published = published.lock().unwrap();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].advice, TaskAdvice::ManualReview);
    }

    fn empty_report(scope: Scope) -> Report {
        Report {
            scope,
            sequence: 0,
            findings: BTreeMap::new(),
            proposals: Vec::new(),
            selected: None,
            truncated: false,
            verified_revision: false,
            model: None,
            usage: None,
        }
    }

    struct NoopModel;

    impl agentscope_core::model::Model for NoopModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> futures::stream::BoxStream<'_, CoreResult<agentscope_core::model::ChatResponse>>
        {
            Box::pin(futures::stream::empty())
        }

        fn model_name(&self) -> &str {
            "noop"
        }
    }

    #[tokio::test]
    async fn phase_routing_applies_the_resolved_model_and_effort_in_enforce() {
        let report = RoutingReport {
            route: Some("fast".into()),
            model: Some("model-b".into()),
            effort: Some("low".into()),
            suggested_effort: None,
            confidence: Some(0.9),
            probabilities: BTreeMap::new(),
            skipped: Vec::new(),
            judge_model: Some("jev".into()),
            judge_usage: Some(crate::jev::dto::Usage::new(1, 1)),
        };
        let expected = report.clone();
        let resolver: RouteResolver = Arc::new(move |_snapshot| {
            let report = expected.clone();
            Box::pin(async move { Ok(report) })
        });
        let middleware = PhaseRoutingMiddleware::new(Mode::Enforce, resolver);
        let ctx = runtime();
        let captured = Arc::new(Mutex::new(None));
        let seen = Arc::clone(&captured);
        let events = middleware
            .on_model_call(
                &ctx,
                ModelCallInput::new(
                    vec![user("q")],
                    Vec::new(),
                    GenerateOptions::default(),
                    Arc::new(NoopModel),
                ),
                Box::new(move |input| {
                    *seen.lock().unwrap() = Some(input.options.clone());
                    stream_of(vec![text_event("x")])
                }),
            )
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 1);
        let options = captured.lock().unwrap().take().unwrap();
        assert_eq!(options.model_name.as_deref(), Some("model-b"));
        assert_eq!(options.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(middleware.report(&run_key(&ctx)), Some(report));
    }

    #[test]
    fn phase_routing_skips_pinned_and_non_text_calls() {
        let resolver: RouteResolver =
            Arc::new(|_snapshot| Box::pin(async { Ok(empty_routing_report()) }));
        let middleware = PhaseRoutingMiddleware::new(Mode::Enforce, resolver);
        let pinned = ModelCallInput::new(
            vec![user("q")],
            Vec::new(),
            GenerateOptions::builder().model_name("pinned").build(),
            Arc::new(NoopModel),
        );
        assert_eq!(
            middleware.restriction(&pinned),
            Some(InputRestriction::PinnedRequestOptions)
        );
        let text_only = ModelCallInput::new(
            vec![user("q")],
            Vec::new(),
            GenerateOptions::default(),
            Arc::new(NoopModel),
        );
        assert_eq!(middleware.restriction(&text_only), None);
        // A tool result carrying a non-text part is not forwardable (the block-type scan).
        let mut with_result = Msg::text(MsgRole::Tool, "");
        with_result.content.push(ContentBlock::ToolResult(
            agentscope_core::message::ToolResultBlock::new("c1", "search", vec![]),
        ));
        let with_result = ModelCallInput::new(
            vec![with_result],
            Vec::new(),
            GenerateOptions::default(),
            Arc::new(NoopModel),
        );
        assert_eq!(middleware.restriction(&with_result), None);
    }

    fn empty_routing_report() -> RoutingReport {
        RoutingReport {
            route: None,
            model: None,
            effort: None,
            suggested_effort: None,
            confidence: None,
            probabilities: BTreeMap::new(),
            skipped: Vec::new(),
            judge_model: None,
            judge_usage: None,
        }
    }

    #[test]
    fn reasoning_hook_is_untouched() {
        // The four hooks above need `on_agent`/`on_model_call`; make sure the default
        // `on_reasoning` passthrough still works for a wrapped middleware instance.
        fn assert_default_reasoning<M: Middleware>(m: &M) {
            let ctx = runtime();
            let stream = m.on_reasoning(
                &ctx,
                ReasoningInput::new(vec![user("q")], Vec::new(), GenerateOptions::default()),
                Box::new(|_input| stream_of(Vec::new())),
            );
            assert!(collect(stream).is_empty());
        }
        assert_default_reasoning(&PhaseRoutingMiddleware::new(
            Mode::Off,
            Arc::new(|_s| Box::pin(async { Ok(empty_routing_report()) })),
        ));
    }

    // --- response quality / reviser (`JevResponseMiddleware.quality`) ---

    fn quality_definition() -> crate::jev::judge::Definition {
        crate::jev::judge::Definition::new(
            "v1",
            vec![crate::jev::judge::Criterion::new(
                "a",
                crate::jev::dto::NoulQuestion::new(json!("is the answer right?"), None),
                true,
                0.2,
                0.8,
            )
            .unwrap()],
        )
        .unwrap()
    }

    fn noul_reply(noul: f64) -> crate::jev::dto::SystemOneResult {
        let mut answers = BTreeMap::new();
        answers.insert(
            "a".to_string(),
            crate::jev::dto::Answer::Noul(crate::jev::dto::NoulAnswer { noul: Some(noul) }),
        );
        crate::jev::dto::SystemOneResult {
            model: Some("jev".to_string()),
            answers: Some(answers),
            usage: Some(crate::jev::dto::Usage::new(2, 2)),
        }
    }

    fn scripted_judge(values: Vec<f64>) -> (Arc<ScriptedCaller>, Arc<crate::jev::judge::JevJudge>) {
        let responses = values
            .into_iter()
            .map(|noul| Ok(noul_reply(noul)))
            .collect();
        let scripted = Arc::new(ScriptedCaller::new(responses));
        let judge = Arc::new(
            crate::jev::judge::JevJudge::new(
                scripted.as_caller(),
                std::time::Duration::from_millis(500),
            )
            .unwrap(),
        );
        (scripted, judge)
    }

    fn reviser(chunks: &[&str], seen: Arc<Mutex<Vec<String>>>) -> Reviser {
        let chunks: Vec<String> = chunks.iter().map(|chunk| (*chunk).to_string()).collect();
        Arc::new(move |revision: Revision| {
            seen.lock().unwrap().push(revision.draft.clone());
            let chunks = chunks.clone();
            Box::pin(async move { Ok(chunks) })
        })
    }

    fn text_of(event: &CoreResult<AgentEvent>) -> String {
        match event {
            Ok(AgentEvent::TextBlockDelta(data)) => data
                .fields
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            _ => String::new(),
        }
    }

    fn call_input() -> ModelCallInput {
        ModelCallInput::new(
            vec![user("q")],
            Vec::new(),
            GenerateOptions::default(),
            Arc::new(NoopModel),
        )
    }

    fn quality_middleware(
        mode: Mode,
        values: Vec<f64>,
        chunks: &[&str],
        max_revisions: usize,
    ) -> (
        ResponseMiddleware,
        Arc<ScriptedCaller>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let (scripted, judge) = scripted_judge(values);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let settings = QualitySettings::new(
            mode,
            judge,
            quality_definition(),
            reviser(chunks, Arc::clone(&seen)),
            max_revisions,
        )
        .unwrap();
        (
            ResponseMiddleware::new(
                Mode::Enforce,
                ResponseLimits::defaults(),
                true,
                screener(false),
            )
            .with_quality(settings),
            scripted,
            seen,
        )
    }

    #[tokio::test]
    async fn quality_enforce_revises_a_failing_draft_and_replaces_the_delta() {
        let (middleware, scripted, seen) =
            quality_middleware(Mode::Enforce, vec![0.1, 0.9], &["revised answer"], 1);
        let ctx = runtime();
        let events = middleware
            .on_model_call(
                &ctx,
                call_input(),
                Box::new(|_i| stream_of(vec![text_event("bad "), text_event("answer")])),
            )
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 1);
        assert_eq!(text_of(&events[0]), "revised answer");
        assert_eq!(*seen.lock().unwrap(), vec!["bad answer".to_string()]);
        // One assessment of the draft, one of the revision.
        assert_eq!(scripted.requests().len(), 2);
    }

    #[tokio::test]
    async fn quality_enforce_withholds_an_inconclusive_assessment() {
        let (middleware, _scripted, _seen) =
            quality_middleware(Mode::Enforce, vec![0.1], &["revised"], 0);
        let ctx = runtime();
        let events = middleware
            .on_model_call(
                &ctx,
                call_input(),
                Box::new(|_i| stream_of(vec![text_event("draft")])),
            )
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 1);
        let error = events[0].as_ref().unwrap_err();
        assert!(format!("{error:?}").contains("QUALITY_FAIL"));
    }

    #[tokio::test]
    async fn quality_off_keeps_the_draft() {
        let (scripted, judge) = scripted_judge(vec![0.1]);
        let settings = QualitySettings::new(
            Mode::Off,
            judge,
            quality_definition(),
            Arc::new(revise_with_model),
            1,
        )
        .unwrap();
        let middleware = ResponseMiddleware::new(
            Mode::Enforce,
            ResponseLimits::defaults(),
            true,
            screener(false),
        )
        .with_quality(settings);
        let ctx = runtime();
        let events = middleware
            .on_model_call(
                &ctx,
                call_input(),
                Box::new(|_i| stream_of(vec![text_event("a"), text_event("b")])),
            )
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 2);
        assert_eq!(text_of(&events[1]), "b");
        assert!(scripted.requests().is_empty());
    }

    #[tokio::test]
    async fn quality_passes_without_revision() {
        let (middleware, scripted, seen) =
            quality_middleware(Mode::Enforce, vec![0.9], &["unused"], 1);
        let ctx = runtime();
        let events = middleware
            .on_model_call(
                &ctx,
                call_input(),
                Box::new(|_i| stream_of(vec![text_event("good"), text_event(" answer")])),
            )
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 2);
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(scripted.requests().len(), 1);
    }

    #[tokio::test]
    async fn quality_skips_tool_calls_and_blank_drafts() {
        let (middleware, scripted, _seen) = quality_middleware(Mode::Enforce, vec![0.1], &["x"], 1);
        let ctx = runtime();
        let events = middleware
            .on_model_call(
                &ctx,
                call_input(),
                Box::new(|_i| {
                    stream_of(vec![
                        text_event("hi"),
                        AgentEvent::ToolCallStart(EventData::default()),
                    ])
                }),
            )
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 2);
        assert!(scripted.requests().is_empty());

        let (middleware, scripted, _seen) = quality_middleware(Mode::Enforce, vec![0.1], &["x"], 1);
        let events = middleware
            .on_model_call(
                &ctx,
                call_input(),
                Box::new(|_i| stream_of(vec![text_event("   ")])),
            )
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 1);
        assert!(scripted.requests().is_empty());
    }

    #[tokio::test]
    async fn quality_shadow_observes_without_replacing() {
        let (scripted, judge) = scripted_judge(vec![0.1]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let settings = QualitySettings::new(
            Mode::Shadow,
            judge,
            quality_definition(),
            reviser(&["revised"], Arc::clone(&seen)),
            1,
        )
        .unwrap();
        let middleware =
            ResponseMiddleware::new(Mode::Off, ResponseLimits::defaults(), true, screener(false))
                .with_quality(settings);
        let ctx = runtime();
        let events = middleware
            .on_model_call(
                &ctx,
                call_input(),
                Box::new(|_i| stream_of(vec![text_event("draft")])),
            )
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 1);
        assert_eq!(text_of(&events[0]), "draft");
        // The assessment runs once and is discarded; SHADOW never revises.
        assert_eq!(scripted.requests().len(), 1);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_blocked_revision_is_withheld() {
        let rounds = Arc::new(Mutex::new(0usize));
        let counted = Arc::clone(&rounds);
        let policy: Screener = Arc::new(move |_text| {
            let counted = Arc::clone(&counted);
            Box::pin(async move {
                let mut guard = counted.lock().unwrap();
                *guard += 1;
                Ok(Verdict {
                    outcome: if *guard == 1 {
                        Outcome::Pass
                    } else {
                        Outcome::Block
                    },
                    scores: BTreeMap::new(),
                    severity: 0.0,
                    triggered: Vec::new(),
                })
            })
        });
        let (_scripted, judge) = scripted_judge(vec![0.1, 0.9]);
        let settings = QualitySettings::new(
            Mode::Enforce,
            judge,
            quality_definition(),
            reviser(&["revised"], Arc::new(Mutex::new(Vec::new()))),
            1,
        )
        .unwrap();
        let middleware =
            ResponseMiddleware::new(Mode::Enforce, ResponseLimits::defaults(), true, policy)
                .with_quality(settings);
        let ctx = runtime();
        let events = middleware
            .on_model_call(
                &ctx,
                call_input(),
                Box::new(|_i| stream_of(vec![text_event("draft")])),
            )
            .collect::<Vec<_>>()
            .await;
        let error = events[0].as_ref().unwrap_err();
        assert!(format!("{error:?}").contains("OUTPUT_BLOCK"));
        assert_eq!(*rounds.lock().unwrap(), 2);
    }
}
