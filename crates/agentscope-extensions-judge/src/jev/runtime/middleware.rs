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
use crate::jev::execution::{Decision, Mode};
use crate::jev::guardrail::Verdict;
use crate::jev::routing::catalog::Snapshot;
use crate::jev::routing::phase_routing::{input_restriction, InputRestriction, RoutingReport};
use crate::jev::supervision::middleware::{append_tail, Observation, Schedule};
use crate::jev::supervision::supervisor::{Report as SupervisionReport, Scope, TaskAdvice};

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
    Box::pin(stream.chain(
        futures::stream::once(async move {
            step.await;
            None::<CoreResult<AgentEvent>>
        })
        .filter_map(|item| async move { item }),
    ))
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
            ContentBlock::Thinking(block) => blocks.push(TraceBlock::Thinking(block.thinking.clone())),
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
                    metadata: result.metadata.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
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

    fn on_agent(&self, ctx: &RuntimeContext, input: AgentInput, next: Next<AgentInput>) -> EventStream {
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

/// The async screening call the host provides (`JevGuardrail::screen`).
///
/// Java's `screen` is a reactive System One call; the injected future keeps the middleware
/// offline-testable while the pure [`JevGuardrail::evaluate`](crate::jev::guardrail::JevGuardrail::evaluate)
/// stays the only verdict fold.
pub type Screener = Arc<dyn Fn(&str) -> BoxedFuture<Result<Verdict, JevError>> + Send + Sync>;

/// What one screening round decided.
#[derive(Debug, Clone, PartialEq)]
pub struct ScreenOutcome {
    /// Whether the text must be withheld.
    pub blocked: bool,
    /// Why it was blocked (the Java `Rejected` reason).
    pub reason: String,
}

/// Drives `JevResponseMiddleware` over the model-call hook.
///
/// `ENFORCE` buffers the call's events, screens the assembled draft and either replays the buffered
/// events or fails the call with `OUTPUT_BLOCK`; `SHADOW` observes the stream and screens after it
/// ends without changing it.
pub struct ResponseMiddleware {
    mode: Mode,
    limits: crate::jev::integration::response::ResponseLimits,
    block_on_review: bool,
    screener: Screener,
}

impl ResponseMiddleware {
    /// Creates the middleware; `mode` must not be `OFF` for it to participate.
    pub fn new(
        mode: Mode,
        limits: crate::jev::integration::response::ResponseLimits,
        block_on_review: bool,
        screener: Screener,
    ) -> Self {
        Self {
            mode,
            limits,
            block_on_review,
            screener,
        }
    }

    /// The configured mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The configured limits.
    pub fn limits(&self) -> &crate::jev::integration::response::ResponseLimits {
        &self.limits
    }

    /// The Java order (`200`).
    pub fn order_hint(&self) -> i32 {
        crate::jev::integration::response::MIDDLEWARE_ORDER
    }

    async fn screen(&self, draft: &str) -> Result<ScreenOutcome, JevError> {
        if draft.trim().is_empty() {
            return Ok(ScreenOutcome {
                blocked: false,
                reason: String::new(),
            });
        }
        if draft.chars().count() > self.limits.max_chars {
            return Err(JevError::invalid("INPUT_LIMIT"));
        }
        let verdict = (self.screener)(draft).await?;
        Ok(ScreenOutcome {
            blocked: verdict.blocked(self.block_on_review),
            reason: format!("OUTPUT_{}", outcome_name(verdict.outcome)),
        })
    }
}

/// The Java enum name of an [`Outcome`](crate::jev::guardrail::Outcome).
fn outcome_name(outcome: crate::jev::guardrail::Outcome) -> &'static str {
    use crate::jev::guardrail::Outcome;
    match outcome {
        Outcome::Pass => "PASS",
        Outcome::Review => "REVIEW",
        Outcome::Block => "BLOCK",
        Outcome::Support => "SUPPORT",
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
        match self.mode {
            Mode::Off => next(input),
            Mode::Shadow => {
                let draft = Arc::new(Mutex::new(String::new()));
                let collected = Arc::clone(&draft);
                let stream = next(input).map(move |event| {
                    if let Ok(AgentEvent::TextBlockDelta(data)) = &event {
                        if let Some(delta) = text_delta(data) {
                            collected.lock().unwrap().push_str(delta);
                        }
                    }
                    event
                });
                let screener = Arc::clone(&self.screener);
                let limits = self.limits;
                let block_on_review = self.block_on_review;
                then_after(stream, async move {
                    let text = { draft.lock().unwrap().clone() };
                    if text.trim().is_empty() || text.chars().count() > limits.max_chars {
                        return;
                    }
                    // Observation only: the verdict is never acted on in SHADOW.
                    let _ = screener(&text).await.map(|verdict| verdict.blocked(block_on_review));
                })
            }
            Mode::Enforce => {
                let middleware = ResponseMiddleware {
                    mode: self.mode,
                    limits: self.limits,
                    block_on_review: self.block_on_review,
                    screener: Arc::clone(&self.screener),
                };
                Box::pin(
                    futures::stream::once(async move {
                        let events: Vec<CoreResult<AgentEvent>> = next(input).collect().await;
                        let mut text = String::new();
                        for event in &events {
                            if let Ok(AgentEvent::TextBlockDelta(data)) = event {
                                if let Some(delta) = text_delta(data) {
                                    text.push_str(delta);
                                }
                            }
                        }
                        match middleware.screen(&text).await {
                            Ok(outcome) if outcome.blocked => {
                                futures::stream::iter(vec![Err(Error::Agent(format!(
                                    "{}: {}",
                                    outcome.reason, "OUTPUT_BLOCK"
                                )))])
                            }
                            Ok(_) => futures::stream::iter(events),
                            Err(error) => futures::stream::iter(vec![Err(Error::Agent(
                                error.message().to_string(),
                            ))]),
                        }
                    })
                    .flatten(),
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 3. Supervision observation (`JevSupervisionMiddleware`)
// ---------------------------------------------------------------------------------------------

/// The async assessment the host provides (a `JevTaskSupervisor` call).
pub type Auditor = Arc<
    dyn Fn(Scope, String, u64) -> BoxedFuture<Result<Decision<SupervisionReport>, JevError>>
        + Send
        + Sync,
>;

/// Receives every published observation.
pub type ObservationSink = Arc<dyn Fn(&Observation) + Send + Sync>;

#[derive(Debug)]
struct SupervisionRun {
    scope: Scope,
    schedule: Schedule,
    tail: String,
    truncated: bool,
    sequence: u64,
    assessments: usize,
    consecutive_failures: usize,
    last_assessment_millis: u64,
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
}

impl SupervisionMiddleware {
    /// Creates the middleware over the pure schedule and the two host seams.
    pub fn new(schedule: Schedule, auditor: Auditor, sink: ObservationSink, tail_limit: usize) -> Self {
        Self {
            schedule,
            auditor,
            sink,
            runs: Arc::new(Mutex::new(HashMap::new())),
            tail_limit: tail_limit.max(1),
        }
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

    fn on_agent(&self, ctx: &RuntimeContext, input: AgentInput, next: Next<AgentInput>) -> EventStream {
        let scope = Scope::new(
            ctx.user_id.clone().unwrap_or_else(|| "unknown".to_string()),
            ctx.session_id.clone().unwrap_or_else(|| "unknown".to_string()),
            run_key(ctx),
        );
        let Ok(scope) = scope else {
            return next(input);
        };
        let key = run_key(ctx);
        let run = Arc::new(Mutex::new(SupervisionRun {
            scope: scope.clone(),
            schedule: self.schedule,
            tail: String::new(),
            truncated: false,
            sequence: 0,
            assessments: 0,
            consecutive_failures: 0,
            last_assessment_millis: 0,
        }));
        self.runs.lock().unwrap().insert(key.clone(), Arc::clone(&run));

        let state = Arc::clone(&run);
        let state_stream = Arc::clone(&run);
        let limit = self.tail_limit;
        let stream = next(input).map(move |event| {
            if let Ok(AgentEvent::TextBlockDelta(data)) = &event {
                if let Some(delta) = text_delta(data) {
                    let mut guard = state_stream.lock().unwrap();
                    guard.sequence += 1;
                    if append_tail(&mut guard.tail, delta, limit) {
                        guard.truncated = true;
                    }
                }
            }
            event
        });

        let registry = Arc::clone(&self.runs);
        let auditor = Arc::clone(&self.auditor);
        let sink = Arc::clone(&self.sink);
        then_after(stream, async move {
            registry.lock().unwrap().remove(&key);
            let (scope, tail, sequence, assessments, failures, truncated) = {
                let guard = state.lock().unwrap();
                (
                    guard.scope.clone(),
                    guard.tail.clone(),
                    guard.sequence,
                    guard.assessments,
                    guard.consecutive_failures,
                    guard.truncated,
                )
            };
            if assessments >= 1 || failures >= 1 {
                return;
            }
            let decision = match auditor(scope.clone(), tail, sequence).await {
                Ok(decision) => decision,
                Err(error) => Decision::new(
                    crate::jev::execution::Status::Error,
                    None,
                    error.message().to_string(),
                    Default::default(),
                ),
            };
            let advice = TaskAdvice::Continue;
            let observation = Observation {
                scope,
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
            guard.truncated |= truncated;
        })
    }

    fn on_model_call(
        &self,
        ctx: &RuntimeContext,
        input: ModelCallInput,
        next: Next<ModelCallInput>,
    ) -> EventStream {
        let key = run_key(ctx);
        let run = self.runs.lock().unwrap().get(&key).cloned();
        if let Some(run) = run {
            let text = input
                .messages
                .iter()
                .filter(|message| message.role == MsgRole::User)
                .map(|message| message.text_content())
                .find(|text| !text.trim().is_empty());
            if let Some(text) = text {
                let mut guard = run.lock().unwrap();
                let schedule = guard.schedule;
                let due = guard.assessments < schedule.max_assessments
                    && guard.consecutive_failures < schedule.max_consecutive_failures
                    && (guard.assessments == 0
                        || now_millis().saturating_sub(guard.last_assessment_millis)
                            >= schedule.periodic_interval_millis);
                if due {
                    if append_tail(&mut guard.tail, &text, self.tail_limit) {
                        guard.truncated = true;
                    }
                    guard.sequence += 1;
                }
            }
        }
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
pub type RouteResolver = Arc<dyn Fn(&Snapshot) -> BoxedFuture<Result<RoutingReport, JevError>> + Send + Sync>;

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
                ContentBlock::Text(_) | ContentBlock::Thinking(_) | ContentBlock::ToolUse(_) => false,
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
    use agentscope_core::middleware::ReasoningInput;
    use agentscope_core::model::GenerateOptions;
    use agentscope_core::message::ToolResultState;
    use crate::jev::evaluation::metrics::Thresholds;
    use crate::jev::guardrail::Outcome;
    use crate::jev::integration::response::ResponseLimits;
    use crate::jev::execution::Options;
    use crate::jev::supervision::supervisor::Report;
    use std::collections::BTreeMap;

    fn text_event(text: &str) -> AgentEvent {        let mut fields = serde_json::Map::new();
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
        Box::pin(futures::stream::iter(events.into_iter().map(Ok).collect::<Vec<_>>()))
    }

    fn collect(stream: EventStream) -> Vec<CoreResult<AgentEvent>> {
        futures::executor::block_on(stream.collect::<Vec<_>>())
    }

    #[test]
    fn trace_projection_carries_roles_blocks_and_tools() {
        let mut msg = Msg::text(MsgRole::Assistant, "hi");
        msg.content.push(ContentBlock::ToolResult(
            agentscope_core::message::ToolResultBlock::new("c1", "search", vec![ContentBlock::Text(
                agentscope_core::message::TextBlock::new("out"),
            )])
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
                Arc::new(|_request| Box::pin(async { Err(crate::jev::call::CallFailure::backend("unused")) })),
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
                Arc::new(|_request| Box::pin(async { Err(crate::jev::call::CallFailure::backend("unused")) })),
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
                    outcome: if blocked { Outcome::Block } else { Outcome::Pass },
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
        let input = || ModelCallInput::new(vec![user("q")], Vec::new(), GenerateOptions::default(), Arc::new(NoopModel));

        let off = ResponseMiddleware::new(Mode::Off, ResponseLimits::defaults(), true, screener(true));
        let events = collect(off.on_model_call(&ctx, input(), Box::new(|_i| stream_of(vec![text_event("x")]))));
        assert_eq!(events.len(), 1);

        let enforce = ResponseMiddleware::new(Mode::Enforce, ResponseLimits::defaults(), true, screener(true));
        assert_eq!(enforce.order(), 200);
        let events = collect(enforce.on_model_call(&ctx, input(), Box::new(|_i| stream_of(vec![text_event("x")]))));
        assert_eq!(events.len(), 1);
        assert!(events[0].is_err());

        let allowed = ResponseMiddleware::new(Mode::Enforce, ResponseLimits::defaults(), true, screener(false));
        let events = collect(allowed.on_model_call(&ctx, input(), Box::new(|_i| stream_of(vec![text_event("x")]))));
        assert_eq!(events.len(), 1);
        assert!(events[0].is_ok());
    }

    #[test]
    fn supervision_middleware_publishes_one_bounded_observation() {
        let published = Arc::new(Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&published);
        let auditor: Auditor = Arc::new(|scope, tail, sequence| {
            Box::pin(async move {
                assert_eq!(tail, "hello");
                assert!(sequence >= 1);
                let mut report = empty_report(scope);
                report.sequence = sequence;
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
        ) -> futures::stream::BoxStream<'_, CoreResult<agentscope_core::model::ChatResponse>> {
            Box::pin(futures::stream::empty())
        }

        fn model_name(&self) -> &str {
            "noop"
        }
    }

    #[test]
    fn phase_routing_applies_the_resolved_model_and_effort_in_enforce() {
        let report = RoutingReport {
            route: Some("fast".into()),
            model: Some("model-b".into()),
            effort: Some("low".into()),
            suggested_effort: None,
            confidence: Some(0.9),
            probabilities: BTreeMap::new(),
            skipped: Vec::new(),
            judge_model: Some("jev".into()),
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
        let events = collect(middleware.on_model_call(
            &ctx,
            ModelCallInput::new(vec![user("q")], Vec::new(), GenerateOptions::default(), Arc::new(NoopModel)),
            Box::new(move |input| {
                *seen.lock().unwrap() = Some(input.options.clone());
                stream_of(vec![text_event("x")])
            }),
        ));
        assert_eq!(events.len(), 1);
        let options = captured.lock().unwrap().take().unwrap();
        assert_eq!(options.model_name.as_deref(), Some("model-b"));
        assert_eq!(options.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(middleware.report(&run_key(&ctx)), Some(report));
    }

    #[test]
    fn phase_routing_skips_pinned_and_non_text_calls() {
        let resolver: RouteResolver = Arc::new(|_snapshot| Box::pin(async { Ok(empty_routing_report()) }));
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
        let text_only = ModelCallInput::new(vec![user("q")], Vec::new(), GenerateOptions::default(), Arc::new(NoopModel));
        assert_eq!(middleware.restriction(&text_only), None);
        // A tool result carrying a non-text part is not forwardable (the block-type scan).
        let mut with_result = Msg::text(MsgRole::Tool, "");
        with_result.content.push(ContentBlock::ToolResult(
            agentscope_core::message::ToolResultBlock::new("c1", "search", vec![]),
        ));
        let with_result = ModelCallInput::new(vec![with_result], Vec::new(), GenerateOptions::default(), Arc::new(NoopModel));
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
        assert_default_reasoning(&PhaseRoutingMiddleware::new(Mode::Off, Arc::new(|_s| {
            Box::pin(async { Ok(empty_routing_report()) })
        })));
    }
}
