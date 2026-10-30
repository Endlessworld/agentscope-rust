//! The three opt-in Decisions middlewares (Java `openaidecisions/example`).
//!
//! Java's package is named `example` but the classes are runtime behaviour, not samples: the
//! `31c7db8c9` commit added them so an application can *apply* a typed judgement to the agent
//! loop, and the Spring Boot starter binds them like any other middleware. They are ported here as
//! `openai_decisions::middleware`:
//!
//! * [`ModelRouterMiddleware`] — `OpenAIDecisionsModelRouterMiddleware` (`ON_AGENT` +
//!   `ON_MODEL_CALL`): one model choice per invocation, gated by a confidence threshold and by a
//!   request-compatibility predicate.
//! * [`ToolSelectionMiddleware`] — `OpenAIDecisionsToolSelectionMiddleware` (`ON_REASONING`):
//!   scores every optional tool, keeps the essential ones, and leaves the tool set untouched when
//!   any score is ambiguous.
//! * [`AutoModeMiddleware`] — `OpenAIDecisionsAutoModeMiddleware` (`ON_ACTING`): assesses guarded
//!   tool calls before dispatch; a refusal or a failed assessment denies the call, an accepted one
//!   still goes through core permissions. Denials record a `DENIED` tool result and emit the
//!   `TOOL_RESULT_*` lifecycle.
//!
//! All three are disabled until explicitly enabled, every network call goes through the injected
//! [`DecisionCaller`] seam (production: [`caller_from_client`]), and `fail_open` decides whether a
//! transport failure falls back to the unjudged behaviour (routing/selection default `true`,
//! guard default `false`, exactly like Java's builders).
//!
//! Documented differences (tracker §12.3):
//!
//! * Java keeps the routing decision in Reactor's `Context` and reads it back with the static
//!   `OpenAIDecisionsModelRouterMiddleware.decision(ctx)`. `&RuntimeContext` is immutable here, so
//!   the middleware owns a `run_id`/`session_id` keyed map and exposes [`ModelRouterMiddleware::decision`].
//! * Denied tool results are recorded through [`ActingInput::results`] (the core persists them as
//!   the tool message) instead of appending to `AgentState.context` directly.
//! * Java wraps the compatibility predicate call in `try/catch (RuntimeException)`; a Rust `Fn`
//!   closure cannot unwind through the middleware safely, so a panic is not translated into
//!   "incompatible".

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result as CoreResult};
use agentscope_core::event::{AgentEvent, AgentEventType};
use agentscope_core::message::{
    ContentBlock, JsonMap, Msg, TextBlock, ToolCallState, ToolResultBlock, ToolResultState,
    ToolUseBlock,
};
use agentscope_core::middleware::{
    ActingInput, AgentInput, EventStream, ExtensionPoint, Middleware, ModelCallInput, Next,
    ReasoningInput,
};
use agentscope_core::model::{Model, ToolChoice, ToolSchema};
use futures::StreamExt;
use serde_json::{json, Value};

use crate::openai_decisions::client::OpenAIDecisionsClient;
use crate::openai_decisions::dto::{
    DecisionAnswer, DecisionOption, DecisionQuestion, DecisionRequest, DecisionResult,
    DecisionValue,
};
use crate::openai_decisions::error::DecisionsError;
use crate::openai_decisions::support::{
    json as to_json, latest_user_text, recent_messages, require_probability, run_key, MAX_QUESTIONS,
};

/// A boxed, `Send` future returned by a [`DecisionCaller`].
pub type DecisionFuture =
    Pin<Box<dyn Future<Output = Result<DecisionResult, DecisionsError>> + Send + 'static>>;

/// One Decisions call as an async seam
/// (Java `Function<DecisionRequest, Mono<DecisionResult>>`).
pub type DecisionCaller = Arc<dyn Fn(DecisionRequest) -> DecisionFuture + Send + Sync>;

/// Adapts an [`OpenAIDecisionsClient`] to the caller seam.
pub fn caller_from_client(client: OpenAIDecisionsClient) -> DecisionCaller {
    Arc::new(move |request| {
        let client = client.clone();
        Box::pin(async move { client.decide(&request).await })
    })
}

/// The routing choice question name (`OpenAIDecisionsModelRouterMiddleware` uses `models_0`).
pub const ROUTING_QUESTION: &str = "models_0";

/// Default routing instructions (`Builder.instructions`).
pub const DEFAULT_ROUTING_INSTRUCTIONS: &str =
    "Choose the best model for this user request using the supplied criteria.";

/// Default routing confidence threshold (`Builder.confidenceThreshold`).
pub const DEFAULT_ROUTING_CONFIDENCE: f64 = 0.8;

/// Default tool-selection limits (`Builder`).
pub const DEFAULT_ALWAYS_INCLUDE_TOOLS: [&str; 3] = [
    "load_skill_through_path",
    "reset_tools",
    "generate_response",
];

/// Default number of selected optional tools (`Builder.maxTools`).
pub const DEFAULT_MAX_TOOLS: usize = 3;

/// Default selection threshold (`Builder.confidenceThreshold`).
pub const DEFAULT_SELECTION_CONFIDENCE: f64 = 0.8;

/// Default rejection threshold (`Builder.rejectionThreshold`).
pub const DEFAULT_REJECTION_THRESHOLD: f64 = 0.2;

/// Default evidence budget in messages (`Builder.maxContextMessages`).
pub const DEFAULT_MAX_CONTEXT_MESSAGES: usize = 8;

/// Default evidence budget in characters (`Builder.maxContextChars`).
pub const DEFAULT_MAX_CONTEXT_CHARS: usize = 8000;

/// Default guard threshold (`Builder.safetyThreshold`).
pub const DEFAULT_SAFETY_THRESHOLD: f64 = 0.8;

/// The message a denied call carries (`OpenAIDecisionsAutoModeMiddleware.DEFAULT_DENY_MESSAGE`).
pub const DEFAULT_DENY_MESSAGE: &str = "Blocked: this tool call did not pass the automatic-execution check. Request review or propose a safer alternative.";

/// The `reply_id` used when the context carries no `AgentState`
/// (`openai-decisions-guard`).
pub const GUARD_REPLY_ID: &str = "openai-decisions-guard";

/// The evidence budget the guard uses for the conversation (`riskRequest`).
pub const GUARD_CONTEXT_MESSAGES: usize = 8;

/// The evidence budget the guard uses for each message (`riskRequest`).
pub const GUARD_CONTEXT_CHARS: usize = 8000;

/// Emits one terminal error instead of delegating.
fn failed(message: impl Into<String>) -> EventStream {
    let message = message.into();
    Box::pin(futures::stream::once(async move {
        Err::<AgentEvent, Error>(Error::Agent(message))
    }))
}

/// Runs `work` once and flattens the stream it returns (`Flux.defer(...)`).
fn deferred<F>(work: F) -> EventStream
where
    F: Future<Output = EventStream> + Send + 'static,
{
    Box::pin(futures::stream::once(work).flatten())
}

// ---------------------------------------------------------------------------------------------
// 1. Model routing (`OpenAIDecisionsModelRouterMiddleware`)
// ---------------------------------------------------------------------------------------------

/// One selectable model plus the criteria the Decision sees
/// (`OpenAIDecisionsModelRouterMiddleware.ModelChoice`).
#[derive(Clone)]
pub struct ModelChoice {
    /// The model to switch to.
    pub model: Arc<dyn Model>,
    /// The criteria shown to the Decision for this choice.
    pub criteria: String,
}

impl ModelChoice {
    /// Creates a choice, rejecting blank criteria (`ModelChoice`'s compact constructor).
    pub fn new(model: Arc<dyn Model>, criteria: impl Into<String>) -> Result<Self, DecisionsError> {
        let criteria = criteria.into();
        if criteria.trim().is_empty() {
            return Err(DecisionsError::invalid("criteria must not be blank"));
        }
        Ok(Self { model, criteria })
    }

    /// The model name, for logging and assertions.
    pub fn model_name(&self) -> &str {
        self.model.model_name()
    }
}

impl std::fmt::Debug for ModelChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelChoice")
            .field("model", &self.model.model_name())
            .field("criteria", &self.criteria)
            .finish()
    }
}

/// The decision one invocation produced (`OpenAIDecisionsModelRouterMiddleware.RoutingDecision`).
#[derive(Clone, Default)]
pub struct RoutingDecision {
    /// The selected model, or `None` for the fallback (no route taken).
    pub model: Option<Arc<dyn Model>>,
    /// The full probability distribution reported for the choice.
    pub probabilities: BTreeMap<String, f64>,
    /// The confidence in the selected choice, when a choice was reported at all.
    pub confidence: Option<f64>,
}

impl RoutingDecision {
    /// The decision that leaves the caller's model alone (`RoutingDecision.fallback()`).
    pub fn fallback() -> Self {
        Self::default()
    }

    /// Whether no route was taken.
    pub fn is_fallback(&self) -> bool {
        self.model.is_none() && self.probabilities.is_empty() && self.confidence.is_none()
    }

    /// The selected model's name, when a route was taken.
    pub fn model_name(&self) -> Option<&str> {
        self.model.as_ref().map(|model| model.model_name())
    }
}

impl std::fmt::Debug for RoutingDecision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RoutingDecision")
            .field("model", &self.model_name())
            .field("probabilities", &self.probabilities)
            .field("confidence", &self.confidence)
            .finish()
    }
}

/// Whether a routed model may serve this call (`Builder.compatible`).
pub type Compatibility = Arc<dyn Fn(&ModelCallInput, &ModelChoice) -> bool + Send + Sync>;

/// The default predicate: only tool-free calls whose structured-output format the model supports.
fn default_compatible(input: &ModelCallInput, choice: &ModelChoice) -> bool {
    input.tools.is_empty()
        && (input.options.response_format.is_none()
            || choice.model.supports_native_structured_output())
}

/// Selects a model once per agent invocation
/// (`OpenAIDecisionsModelRouterMiddleware`); disabled until explicitly enabled.
#[derive(Clone)]
pub struct ModelRouterMiddleware {
    call: DecisionCaller,
    instructions: String,
    confidence_threshold: f64,
    enabled: bool,
    fail_open: bool,
    compatible: Compatibility,
    choices: Vec<(String, ModelChoice)>,
    decisions: Arc<Mutex<HashMap<String, RoutingDecision>>>,
}

impl ModelRouterMiddleware {
    /// Starts building over an injected caller (`builder(Function)`).
    pub fn builder(call: DecisionCaller) -> ModelRouterMiddlewareBuilder {
        ModelRouterMiddlewareBuilder::new(call)
    }

    /// Starts building over a Decisions client (`builder(OpenAIDecisionsClient)`).
    pub fn from_client(client: OpenAIDecisionsClient) -> ModelRouterMiddlewareBuilder {
        ModelRouterMiddlewareBuilder::new(caller_from_client(client))
    }

    /// Whether routing is enabled (`Builder.enabled`, default `false`).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The configured confidence threshold.
    pub fn confidence_threshold(&self) -> f64 {
        self.confidence_threshold
    }

    /// The decision this context currently holds (`decision(ctx)`), scoped like Java's
    /// `RuntimeContext` entry.
    pub fn decision(&self, ctx: &RuntimeContext) -> Option<RoutingDecision> {
        self.decision_for_key(&run_key(ctx))
    }

    /// The decision stored under a [`run_key`].
    pub fn decision_for_key(&self, key: &str) -> Option<RoutingDecision> {
        self.decisions.lock().ok()?.get(key).cloned()
    }

    fn store(&self, key: &str, decision: RoutingDecision) {
        if let Ok(mut decisions) = self.decisions.lock() {
            decisions.insert(key.to_string(), decision);
        }
    }

    /// The one-question request (`models_0`, one option per configured choice).
    fn request(&self, text: &str) -> Result<DecisionRequest, DecisionsError> {
        let options: Vec<DecisionOption> = self
            .choices
            .iter()
            .map(|(name, choice)| {
                DecisionOption::new(
                    DecisionValue::text(name.clone()),
                    Some(choice.criteria.clone()),
                )
            })
            .collect();
        let question = DecisionQuestion::choice(self.instructions.clone(), options)?;
        DecisionRequest::builder()
            .input_text(text)
            .question(ROUTING_QUESTION, question)
            .build()
    }

    /// Folds one reply into a decision; refusals, unknown choices and out-of-range
    /// confidences fall back exactly like `selectedModel`.
    fn selected_model(&self, result: &DecisionResult) -> Result<RoutingDecision, DecisionsError> {
        let Some(DecisionAnswer::Choice {
            choice,
            confidence,
            probabilities,
        }) = result.answers.get(ROUTING_QUESTION)
        else {
            return Ok(RoutingDecision::fallback());
        };
        let DecisionValue::Text(name) = choice else {
            return Ok(RoutingDecision::fallback());
        };
        let Some((_, selected)) = self.choices.iter().find(|(key, _)| key == name) else {
            return Ok(RoutingDecision::fallback());
        };
        let confidence = require_probability(*confidence, "confidence")?;
        let mut distribution = BTreeMap::new();
        for entry in probabilities {
            if let DecisionValue::Text(name) = &entry.value {
                distribution.insert(name.clone(), entry.probability);
            }
        }
        let model = if confidence >= self.confidence_threshold {
            Some(Arc::clone(&selected.model))
        } else {
            None
        };
        Ok(RoutingDecision {
            model,
            probabilities: distribution,
            confidence: Some(confidence),
        })
    }
}

impl Middleware for ModelRouterMiddleware {
    fn order(&self) -> i32 {
        0
    }

    fn active_points(&self) -> Vec<ExtensionPoint> {
        vec![ExtensionPoint::OnAgent, ExtensionPoint::OnModelCall]
    }

    fn on_agent(
        &self,
        ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        let key = run_key(ctx);
        // Java puts the fallback on every invocation, so a reused context never inherits a route.
        self.store(&key, RoutingDecision::fallback());
        let text = latest_user_text(&input.messages);
        if !self.enabled || text.is_empty() {
            return next(input);
        }
        let request = match self.request(&text) {
            Ok(request) => request,
            Err(error) => {
                return if self.fail_open {
                    next(input)
                } else {
                    failed(error.message())
                }
            }
        };
        let this = self.clone();
        deferred(async move {
            match (this.call)(request).await {
                Ok(result) => match this.selected_model(&result) {
                    Ok(decision) => this.store(&key, decision),
                    Err(error) => {
                        if !this.fail_open {
                            return failed(error.message());
                        }
                    }
                },
                Err(error) => {
                    if !this.fail_open {
                        return failed(error.message());
                    }
                }
            }
            next(input)
        })
    }

    fn on_model_call(
        &self,
        ctx: &RuntimeContext,
        input: ModelCallInput,
        next: Next<ModelCallInput>,
    ) -> EventStream {
        if !self.enabled {
            return next(input);
        }
        let key = run_key(ctx);
        let Some(decision) = self.decision_for_key(&key) else {
            return next(input);
        };
        let Some(model) = decision.model else {
            return next(input);
        };
        let Some((_, choice)) = self
            .choices
            .iter()
            .find(|(_, choice)| Arc::ptr_eq(&choice.model, &model))
        else {
            self.store(&key, RoutingDecision::fallback());
            return next(input);
        };
        if !(self.compatible)(&input, choice) {
            self.store(&key, RoutingDecision::fallback());
            return next(input);
        }
        next(ModelCallInput { model, ..input })
    }
}

/// Incremental builder for [`ModelRouterMiddleware`]
/// (`OpenAIDecisionsModelRouterMiddleware.Builder`).
pub struct ModelRouterMiddlewareBuilder {
    call: DecisionCaller,
    instructions: String,
    confidence_threshold: f64,
    enabled: bool,
    fail_open: bool,
    compatible: Option<Compatibility>,
    choices: Vec<(String, ModelChoice)>,
}

impl ModelRouterMiddlewareBuilder {
    /// Creates a builder for an injected caller.
    pub fn new(call: DecisionCaller) -> Self {
        Self {
            call,
            instructions: DEFAULT_ROUTING_INSTRUCTIONS.to_string(),
            confidence_threshold: DEFAULT_ROUTING_CONFIDENCE,
            enabled: false,
            fail_open: true,
            compatible: None,
            choices: Vec::new(),
        }
    }

    /// Adds (or replaces) one named choice (`Builder.choice`).
    ///
    /// Java throws here on blank criteria; a Rust builder cannot short-circuit a chain, so the
    /// same message is reported by [`Self::build`].
    pub fn choice(
        mut self,
        name: impl Into<String>,
        model: Arc<dyn Model>,
        criteria: impl Into<String>,
    ) -> Self {
        let name = name.into();
        let choice = ModelChoice {
            model,
            criteria: criteria.into(),
        };
        match self.choices.iter_mut().find(|(key, _)| key == &name) {
            Some(slot) => slot.1 = choice,
            None => self.choices.push((name, choice)),
        }
        self
    }

    /// Sets the choice instructions.
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }

    /// Sets the confidence threshold.
    pub fn confidence_threshold(mut self, threshold: f64) -> Self {
        self.confidence_threshold = threshold;
        self
    }

    /// Enables routing (default `false`).
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Whether a failure falls back to the caller's model (default `true` for routing).
    pub fn fail_open(mut self, fail_open: bool) -> Self {
        self.fail_open = fail_open;
        self
    }

    /// Overrides the compatibility predicate.
    pub fn compatible(mut self, compatible: Compatibility) -> Self {
        self.compatible = Some(compatible);
        self
    }

    /// Validates and builds.
    pub fn build(self) -> Result<ModelRouterMiddleware, DecisionsError> {
        if !(2..=255).contains(&self.choices.len()) {
            return Err(DecisionsError::invalid(
                "require 2\u{2013}255 model choices",
            ));
        }
        if self.choices.iter().any(|(name, _)| name.trim().is_empty())
            || self.instructions.trim().is_empty()
        {
            return Err(DecisionsError::invalid(
                "choice names and instructions must not be blank",
            ));
        }
        if self
            .choices
            .iter()
            .any(|(_, choice)| choice.criteria.trim().is_empty())
        {
            return Err(DecisionsError::invalid("criteria must not be blank"));
        }
        require_probability(self.confidence_threshold, "confidenceThreshold")?;
        let default_compatibility: Compatibility = Arc::new(default_compatible);
        Ok(ModelRouterMiddleware {
            call: self.call,
            instructions: self.instructions,
            confidence_threshold: self.confidence_threshold,
            enabled: self.enabled,
            fail_open: self.fail_open,
            compatible: self.compatible.unwrap_or(default_compatibility),
            choices: self.choices,
            decisions: Arc::new(Mutex::new(HashMap::new())),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// 2. Tool selection (`OpenAIDecisionsToolSelectionMiddleware`)
// ---------------------------------------------------------------------------------------------

/// Scores optional tools on every reasoning step and keeps the essential ones
/// (`OpenAIDecisionsToolSelectionMiddleware`); filtering is opt-in.
#[derive(Clone)]
pub struct ToolSelectionMiddleware {
    call: DecisionCaller,
    always_include_tools: Vec<String>,
    max_tools: usize,
    confidence_threshold: f64,
    rejection_threshold: f64,
    max_context_messages: usize,
    max_context_chars: usize,
    enabled: bool,
    fail_open: bool,
}

impl ToolSelectionMiddleware {
    /// Starts building over an injected caller (`builder(Function)`).
    pub fn builder(call: DecisionCaller) -> ToolSelectionMiddlewareBuilder {
        ToolSelectionMiddlewareBuilder::new(call)
    }

    /// Starts building over a Decisions client (`builder(OpenAIDecisionsClient)`).
    pub fn from_client(client: OpenAIDecisionsClient) -> ToolSelectionMiddlewareBuilder {
        ToolSelectionMiddlewareBuilder::new(caller_from_client(client))
    }

    /// Whether filtering is enabled (`Builder.enabled`, default `false`).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The configured tool cap.
    pub fn max_tools(&self) -> usize {
        self.max_tools
    }

    /// Scores every optional tool, returning the selected names, or `None` when any score is
    /// ambiguous (`select`).
    async fn select(
        &self,
        optional: &[ToolSchema],
        evidence: &str,
    ) -> Result<Option<Vec<String>>, DecisionsError> {
        let mut scores: Vec<(String, f64)> = Vec::new();
        // Java `Flux.range(...).concatMap(...)`: batches run sequentially, <=400 questions each.
        for batch in optional.chunks(MAX_QUESTIONS) {
            let mut builder = DecisionRequest::builder().input_text(evidence);
            for (index, tool) in batch.iter().enumerate() {
                let detail = to_json(&json!({
                    "name": tool.name,
                    "description": tool.description,
                }));
                builder = builder.question(
                    format!("tool_{index}"),
                    DecisionQuestion::predicate(format!("Is this tool applicable to the user's current request, given the conversation? Tool: {detail}")),
                );
            }
            let result = (self.call)(builder.build()?).await?;
            for (index, tool) in batch.iter().enumerate() {
                let name = format!("tool_{index}");
                let Some(DecisionAnswer::Predicate { probability }) = result.answers.get(&name)
                else {
                    return Err(DecisionsError::invalid("Tool relevance was not answered"));
                };
                let probability = require_probability(*probability, "relevance")?;
                scores.push((tool.name.clone(), probability));
            }
        }
        if scores.iter().any(|(_, probability)| {
            *probability > self.rejection_threshold && *probability < self.confidence_threshold
        }) {
            return Ok(None);
        }
        let mut selected: Vec<(String, f64)> = scores
            .into_iter()
            .filter(|(_, probability)| *probability >= self.confidence_threshold)
            .collect();
        // `sorted(comparingByValue().reversed())`: stable, so ties keep the tool order.
        selected.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        selected.truncate(self.max_tools);
        Ok(Some(selected.into_iter().map(|(name, _)| name).collect()))
    }
}

impl Middleware for ToolSelectionMiddleware {
    fn order(&self) -> i32 {
        0
    }

    fn active_points(&self) -> Vec<ExtensionPoint> {
        vec![ExtensionPoint::OnReasoning]
    }

    fn on_reasoning(
        &self,
        _ctx: &RuntimeContext,
        input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> EventStream {
        if !self.enabled || latest_user_text(&input.messages).is_empty() {
            return next(input);
        }
        let mut required = self.always_include_tools.clone();
        if let Some(ToolChoice::Specific { tool_name }) = input.options.tool_choice.as_ref() {
            required.push(tool_name.clone());
        }
        // Java also skips a tool that is already required, so a forced tool is never judged.
        let optional: Vec<ToolSchema> = input
            .tools
            .iter()
            .filter(|tool| !required.contains(&tool.name))
            .cloned()
            .collect();
        if optional.is_empty() {
            return next(input);
        }
        let evidence = to_json(&json!({
            "messages": recent_messages(
                &input.messages,
                self.max_context_messages,
                self.max_context_chars,
            ),
        }));
        let this = self.clone();
        deferred(async move {
            match this.select(&optional, &evidence).await {
                Ok(Some(selected)) => {
                    let tools = input
                        .tools
                        .iter()
                        .filter(|tool| {
                            required.contains(&tool.name) || selected.contains(&tool.name)
                        })
                        .cloned()
                        .collect();
                    next(ReasoningInput {
                        messages: input.messages,
                        tools,
                        options: input.options,
                    })
                }
                Ok(None) => next(input),
                Err(error) => {
                    if this.fail_open {
                        next(input)
                    } else {
                        failed(error.message())
                    }
                }
            }
        })
    }
}

/// Incremental builder for [`ToolSelectionMiddleware`]
/// (`OpenAIDecisionsToolSelectionMiddleware.Builder`).
pub struct ToolSelectionMiddlewareBuilder {
    call: DecisionCaller,
    always_include_tools: Vec<String>,
    max_tools: usize,
    confidence_threshold: f64,
    rejection_threshold: f64,
    max_context_messages: usize,
    max_context_chars: usize,
    enabled: bool,
    fail_open: bool,
}

impl ToolSelectionMiddlewareBuilder {
    /// Creates a builder with Java's defaults.
    pub fn new(call: DecisionCaller) -> Self {
        Self {
            call,
            always_include_tools: DEFAULT_ALWAYS_INCLUDE_TOOLS
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            max_tools: DEFAULT_MAX_TOOLS,
            confidence_threshold: DEFAULT_SELECTION_CONFIDENCE,
            rejection_threshold: DEFAULT_REJECTION_THRESHOLD,
            max_context_messages: DEFAULT_MAX_CONTEXT_MESSAGES,
            max_context_chars: DEFAULT_MAX_CONTEXT_CHARS,
            enabled: false,
            fail_open: true,
        }
    }

    /// Replaces the always-included tool names (`Builder.alwaysIncludeTools`).
    pub fn always_include_tools<S: AsRef<str>>(
        mut self,
        names: impl IntoIterator<Item = S>,
    ) -> Self {
        self.always_include_tools = names
            .into_iter()
            .map(|name| name.as_ref().to_string())
            .collect();
        self
    }

    /// Sets the number of optional tools that may survive (`Builder.maxTools`).
    pub fn max_tools(mut self, max_tools: usize) -> Self {
        self.max_tools = max_tools;
        self
    }

    /// Sets the selection threshold.
    pub fn confidence_threshold(mut self, threshold: f64) -> Self {
        self.confidence_threshold = threshold;
        self
    }

    /// Sets the ambiguity floor.
    pub fn rejection_threshold(mut self, threshold: f64) -> Self {
        self.rejection_threshold = threshold;
        self
    }

    /// Sets the evidence message budget.
    pub fn max_context_messages(mut self, max_messages: usize) -> Self {
        self.max_context_messages = max_messages;
        self
    }

    /// Sets the evidence character budget.
    pub fn max_context_chars(mut self, max_chars: usize) -> Self {
        self.max_context_chars = max_chars;
        self
    }

    /// Enables filtering (default `false`).
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Whether a failure leaves the tool set alone (default `true`).
    pub fn fail_open(mut self, fail_open: bool) -> Self {
        self.fail_open = fail_open;
        self
    }

    /// Validates and builds.
    pub fn build(self) -> Result<ToolSelectionMiddleware, DecisionsError> {
        require_probability(self.confidence_threshold, "confidenceThreshold")?;
        require_probability(self.rejection_threshold, "rejectionThreshold")?;
        if self.max_tools == 0
            || self.max_context_messages == 0
            || self.max_context_chars == 0
            || self.rejection_threshold >= self.confidence_threshold
            || self
                .always_include_tools
                .iter()
                .any(|name| name.trim().is_empty())
        {
            return Err(DecisionsError::invalid(
                "require positive limits, nonblank tool names and rejection < selection",
            ));
        }
        Ok(ToolSelectionMiddleware {
            call: self.call,
            always_include_tools: self.always_include_tools,
            max_tools: self.max_tools,
            confidence_threshold: self.confidence_threshold,
            rejection_threshold: self.rejection_threshold,
            max_context_messages: self.max_context_messages,
            max_context_chars: self.max_context_chars,
            enabled: self.enabled,
            fail_open: self.fail_open,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// 3. Guarded tool calls (`OpenAIDecisionsAutoModeMiddleware`)
// ---------------------------------------------------------------------------------------------

/// Assesses guarded tool calls before dispatch
/// (`OpenAIDecisionsAutoModeMiddleware`); a refusal or a failed assessment denies the call.
#[derive(Clone)]
pub struct AutoModeMiddleware {
    call: DecisionCaller,
    guarded_tools: Vec<String>,
    safety_threshold: f64,
    deny_message: String,
    enabled: bool,
    fail_open: bool,
}

impl AutoModeMiddleware {
    /// Starts building over an injected caller (`builder(Function)`).
    pub fn builder(call: DecisionCaller) -> AutoModeMiddlewareBuilder {
        AutoModeMiddlewareBuilder::new(call)
    }

    /// Starts building over a Decisions client (`builder(OpenAIDecisionsClient)`).
    pub fn from_client(client: OpenAIDecisionsClient) -> AutoModeMiddlewareBuilder {
        AutoModeMiddlewareBuilder::new(caller_from_client(client))
    }

    /// Whether the guard is enabled (`Builder.enabled`, default `false`).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The message a denied call carries.
    pub fn deny_message(&self) -> &str {
        &self.deny_message
    }

    /// Assesses every guarded call, batch by batch; a failed batch fails open or closed on its
    /// own, exactly like `assessBatch`'s `onErrorResume`.
    async fn assess(&self, messages: &[Msg], guarded: &[ToolUseBlock]) -> Vec<String> {
        let mut allowed = Vec::new();
        for batch in guarded.chunks(MAX_QUESTIONS) {
            match self.assess_batch(messages, batch).await {
                Ok(batch_allowed) => allowed.extend(batch_allowed),
                Err(_) => {
                    if self.fail_open {
                        allowed.extend(batch.iter().map(|call| call.id.clone()));
                    }
                }
            }
        }
        allowed
    }

    async fn assess_batch(
        &self,
        messages: &[Msg],
        batch: &[ToolUseBlock],
    ) -> Result<Vec<String>, DecisionsError> {
        let result = (self.call)(self.risk_request(messages, batch)?).await?;
        let mut allowed = Vec::new();
        for (index, call) in batch.iter().enumerate() {
            let name = format!("tool_{index}");
            if let Some(DecisionAnswer::Predicate { probability }) = result.answers.get(&name) {
                let probability = require_probability(*probability, "safety")?;
                if probability >= self.safety_threshold {
                    allowed.push(call.id.clone());
                }
            }
        }
        Ok(allowed)
    }

    /// The one-request evidence payload (`riskRequest`): bounded messages plus the calls.
    fn risk_request(
        &self,
        messages: &[Msg],
        batch: &[ToolUseBlock],
    ) -> Result<DecisionRequest, DecisionsError> {
        let details: Vec<Value> = batch
            .iter()
            .map(|call| {
                json!({
                    "name": call.name,
                    "input": Value::Object(call.input.clone()),
                })
            })
            .collect();
        let evidence = to_json(&json!({
            "messages": recent_messages(messages, GUARD_CONTEXT_MESSAGES, GUARD_CONTEXT_CHARS),
            "tool_calls": details,
        }));
        let mut builder = DecisionRequest::builder().input_text(evidence);
        for index in 0..batch.len() {
            builder = builder.question(
                format!("tool_{index}"),
                DecisionQuestion::predicate(format!("Is tool_calls[{index}] safe to execute automatically, given the conversation? Consider destructive operations, data loss, irreversible changes and security implications.")),
            );
        }
        builder.build()
    }
}

impl Middleware for AutoModeMiddleware {
    fn order(&self) -> i32 {
        0
    }

    fn active_points(&self) -> Vec<ExtensionPoint> {
        vec![ExtensionPoint::OnActing]
    }

    fn on_acting(
        &self,
        ctx: &RuntimeContext,
        input: ActingInput,
        next: Next<ActingInput>,
    ) -> EventStream {
        if !self.enabled {
            return next(input);
        }
        let ActingInput {
            tool_calls,
            results,
        } = input;
        let guarded: Vec<ToolUseBlock> = tool_calls
            .iter()
            .filter(|call| {
                self.guarded_tools.contains(&call.name) && call.state != ToolCallState::Allowed
            })
            .cloned()
            .collect();
        if guarded.is_empty() {
            return next(ActingInput::with_results(tool_calls, results));
        }
        // Java resolves the agent state; without one the guard cannot judge and applies
        // `failOpen` to the whole batch (`state == null ? failOpen ? guarded : List.of()`).
        let messages = ctx.state.as_ref().map(|state| {
            state
                .lock()
                .map(|state| state.context().to_vec())
                .unwrap_or_default()
        });
        let reply_id = ctx
            .state
            .as_ref()
            .and_then(|state| state.lock().ok().map(|state| state.reply_id().to_string()))
            .unwrap_or_else(|| GUARD_REPLY_ID.to_string());
        let this = self.clone();
        deferred(async move {
            let allowed = match messages {
                Some(messages) => this.assess(&messages, &guarded).await,
                None if this.fail_open => guarded.iter().map(|call| call.id.clone()).collect(),
                None => Vec::new(),
            };
            let denied: Vec<ToolUseBlock> = guarded
                .into_iter()
                .filter(|call| !allowed.contains(&call.id))
                .collect();
            if denied.is_empty() {
                return next(ActingInput::with_results(tool_calls, results));
            }
            let denied_ids: Vec<String> = denied.iter().map(|call| call.id.clone()).collect();
            let passing: Vec<ToolUseBlock> = tool_calls
                .iter()
                .filter(|call| !denied_ids.contains(&call.id))
                .cloned()
                .collect();
            for call in &denied {
                results.record(denied_result(call, &this.deny_message), false);
            }
            let events = denied_events(&denied, &reply_id, &this.deny_message);
            if passing.is_empty() {
                events
            } else {
                // Java concatenates the denial events with the delegated call.
                Box::pin(events.chain(next(ActingInput::with_results(passing, results))))
            }
        })
    }
}

/// The `DENIED` result a blocked call records (`deniedEvents`' `ToolResultBlock.text(...)`).
fn denied_result(call: &ToolUseBlock, message: &str) -> ToolResultBlock {
    ToolResultBlock {
        id: Some(call.id.clone()),
        name: Some(call.name.clone()),
        output: vec![ContentBlock::Text(TextBlock::new(message))],
        metadata: JsonMap::new(),
        state: ToolResultState::Denied,
    }
}

/// The synthetic `TOOL_RESULT_START` / `TEXT_DELTA` / `END(DENIED)` lifecycle per blocked call.
fn denied_events(denied: &[ToolUseBlock], reply_id: &str, message: &str) -> EventStream {
    let mut events: Vec<CoreResult<AgentEvent>> = Vec::new();
    for call in denied {
        let mut base = JsonMap::new();
        base.insert("reply_id".to_string(), json!(reply_id));
        base.insert("tool_call_id".to_string(), json!(call.id));
        base.insert("tool_name".to_string(), json!(call.name));
        events.push(Ok(AgentEvent::with_fields(
            AgentEventType::ToolResultStart,
            base.clone(),
        )));
        let mut delta = base.clone();
        delta.insert("delta".to_string(), json!(message));
        events.push(Ok(AgentEvent::with_fields(
            AgentEventType::ToolResultTextDelta,
            delta,
        )));
        let mut end = base;
        end.insert("state".to_string(), json!("DENIED"));
        events.push(Ok(AgentEvent::with_fields(
            AgentEventType::ToolResultEnd,
            end,
        )));
    }
    Box::pin(futures::stream::iter(events))
}

/// Incremental builder for [`AutoModeMiddleware`]
/// (`OpenAIDecisionsAutoModeMiddleware.Builder`).
pub struct AutoModeMiddlewareBuilder {
    call: DecisionCaller,
    guarded_tools: Vec<String>,
    safety_threshold: f64,
    deny_message: String,
    enabled: bool,
    fail_open: bool,
}

impl AutoModeMiddlewareBuilder {
    /// Creates a builder with Java's defaults (guard disabled, fail closed).
    pub fn new(call: DecisionCaller) -> Self {
        Self {
            call,
            guarded_tools: Vec::new(),
            safety_threshold: DEFAULT_SAFETY_THRESHOLD,
            deny_message: DEFAULT_DENY_MESSAGE.to_string(),
            enabled: false,
            fail_open: false,
        }
    }

    /// Adds one guarded tool (`Builder.guardedTool`).
    pub fn guarded_tool(mut self, name: impl Into<String>) -> Self {
        let name = name.into();
        if !self.guarded_tools.contains(&name) {
            self.guarded_tools.push(name);
        }
        self
    }

    /// Replaces the guarded tool set (`Builder.guardedTools`).
    pub fn guarded_tools<S: AsRef<str>>(mut self, names: impl IntoIterator<Item = S>) -> Self {
        self.guarded_tools = names
            .into_iter()
            .map(|name| name.as_ref().to_string())
            .collect();
        self
    }

    /// Sets the acceptance threshold.
    pub fn safety_threshold(mut self, threshold: f64) -> Self {
        self.safety_threshold = threshold;
        self
    }

    /// Sets the message a denied call carries.
    pub fn deny_message(mut self, message: impl Into<String>) -> Self {
        self.deny_message = message.into();
        self
    }

    /// Enables the guard (default `false`).
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Whether an assessment failure allows the call (default `false`: deny by default).
    pub fn fail_open(mut self, fail_open: bool) -> Self {
        self.fail_open = fail_open;
        self
    }

    /// Validates and builds.
    pub fn build(self) -> Result<AutoModeMiddleware, DecisionsError> {
        require_probability(self.safety_threshold, "safetyThreshold")?;
        if self.guarded_tools.is_empty()
            || self.guarded_tools.iter().any(|name| name.trim().is_empty())
            || self.deny_message.trim().is_empty()
        {
            return Err(DecisionsError::invalid(
                "guardedTools and denyMessage must not be empty",
            ));
        }
        Ok(AutoModeMiddleware {
            call: self.call,
            guarded_tools: self.guarded_tools,
            safety_threshold: self.safety_threshold,
            deny_message: self.deny_message,
            enabled: self.enabled,
            fail_open: self.fail_open,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::message::ToolResultState;
    use agentscope_core::model::{ChatResponse, GenerateOptions};
    use agentscope_core::state::AgentState;
    use futures::stream::BoxStream;
    use serde_json::json;
    use std::collections::BTreeMap;

    use crate::openai_decisions::dto::ChoiceProbability;

    /// A model that never streams: the middleware tests only inspect the routing decision.
    struct StubModel {
        name: &'static str,
        native: bool,
    }

    impl Model for StubModel {
        fn stream(
            &self,
            _messages: Vec<Msg>,
            _tools: Vec<ToolSchema>,
            _options: GenerateOptions,
        ) -> BoxStream<'_, CoreResult<ChatResponse>> {
            Box::pin(futures::stream::empty::<CoreResult<ChatResponse>>())
        }

        fn model_name(&self) -> &str {
            self.name
        }

        fn supports_native_structured_output(&self) -> bool {
            self.native
        }
    }

    fn model(name: &'static str) -> Arc<dyn Model> {
        Arc::new(StubModel { name, native: true })
    }

    fn answering(result: DecisionResult) -> DecisionCaller {
        Arc::new(move |_request: DecisionRequest| {
            let reply: Result<DecisionResult, DecisionsError> = Ok(result.clone());
            Box::pin(async move { reply })
        })
    }

    fn failing(message: &'static str) -> DecisionCaller {
        Arc::new(move |_request: DecisionRequest| {
            let error: Result<DecisionResult, DecisionsError> =
                Err(DecisionsError::api(message, Some(500), true));
            Box::pin(async move { error })
        })
    }

    fn choice_reply(name: &str, confidence: f64, distribution: &[(&str, f64)]) -> DecisionResult {
        let mut answers = BTreeMap::new();
        answers.insert(
            ROUTING_QUESTION.to_string(),
            DecisionAnswer::Choice {
                choice: DecisionValue::text(name),
                confidence,
                probabilities: distribution
                    .iter()
                    .map(|(value, probability)| ChoiceProbability {
                        value: DecisionValue::text(*value),
                        probability: *probability,
                    })
                    .collect(),
            },
        );
        DecisionResult {
            model: "decisions-1".to_string(),
            answers,
            usage: None,
        }
    }

    fn predicate_reply(answers: &[(&str, f64)]) -> DecisionResult {
        let mut replies = BTreeMap::new();
        for (name, probability) in answers {
            replies.insert(
                (*name).to_string(),
                DecisionAnswer::Predicate {
                    probability: *probability,
                },
            );
        }
        DecisionResult {
            model: "decisions-1".to_string(),
            answers: replies,
            usage: None,
        }
    }

    fn refusal_reply(name: &str) -> DecisionResult {
        let mut answers = BTreeMap::new();
        answers.insert(name.to_string(), DecisionAnswer::Refusal);
        DecisionResult {
            model: "decisions-1".to_string(),
            answers,
            usage: None,
        }
    }

    async fn drain(stream: EventStream) -> Vec<AgentEvent> {
        stream
            .filter_map(|item| async move { item.ok() })
            .collect()
            .await
    }

    async fn first_error(stream: EventStream) -> Option<String> {
        let items: Vec<CoreResult<AgentEvent>> = stream.collect().await;
        items
            .into_iter()
            .find_map(|item| item.err().map(|error| error.to_string()))
    }

    fn tool_call(id: &str, name: &str) -> ToolUseBlock {
        ToolUseBlock {
            id: id.to_string(),
            name: name.to_string(),
            input: JsonMap::new(),
            content: None,
            metadata: JsonMap::new(),
            state: ToolCallState::Pending,
        }
    }

    // -----------------------------------------------------------------------------------------
    // Model routing
    // -----------------------------------------------------------------------------------------

    #[tokio::test]
    async fn the_router_selects_once_per_invocation_and_swaps_the_model() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let caller: DecisionCaller = Arc::new(move |request: DecisionRequest| {
            seen.lock().unwrap().push(request.questions().len());
            let reply: Result<DecisionResult, DecisionsError> =
                Ok(choice_reply("slow", 0.9, &[("fast", 0.1), ("slow", 0.9)]));
            Box::pin(async move { reply })
        });
        let middleware = ModelRouterMiddleware::builder(caller)
            .choice("fast", model("fast"), "cheap and quick")
            .choice("slow", model("slow"), "best reasoning")
            .enabled(true)
            .build()
            .unwrap();
        let ctx = RuntimeContext::new().with_session_id("s-route");

        let stream = middleware.on_agent(
            &ctx,
            AgentInput::new(vec![Msg::user("solve this")]),
            Box::new(|_input: AgentInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        assert!(drain(stream).await.is_empty());

        let decision = middleware.decision(&ctx).unwrap();
        assert_eq!(decision.model_name(), Some("slow"));
        assert_eq!(decision.confidence, Some(0.9));
        assert_eq!(decision.probabilities.get("fast"), Some(&0.1));
        assert_eq!(*requests.lock().unwrap(), vec![1]);

        let called = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&called);
        let stream = middleware.on_model_call(
            &ctx,
            ModelCallInput::new(
                vec![Msg::user("hi")],
                Vec::new(),
                GenerateOptions::default(),
                model("fast"),
            ),
            Box::new(move |input: ModelCallInput| -> EventStream {
                sink.lock()
                    .unwrap()
                    .push(input.model.model_name().to_string());
                Box::pin(futures::stream::empty())
            }),
        );
        assert!(drain(stream).await.is_empty());
        assert_eq!(*called.lock().unwrap(), vec!["slow".to_string()]);
    }

    #[tokio::test]
    async fn a_refusal_low_confidence_or_unknown_choice_keeps_the_caller_model() {
        let replies = vec![
            refusal_reply(ROUTING_QUESTION),
            choice_reply("slow", 0.5, &[("fast", 0.5), ("slow", 0.5)]),
            choice_reply("unknown", 0.99, &[("unknown", 0.99)]),
        ];
        for reply in replies {
            let middleware = ModelRouterMiddleware::builder(answering(reply))
                .choice("fast", model("fast"), "cheap")
                .choice("slow", model("slow"), "thorough")
                .enabled(true)
                .build()
                .unwrap();
            let ctx = RuntimeContext::new().with_session_id("s-fallback");
            let stream = middleware.on_agent(
                &ctx,
                AgentInput::new(vec![Msg::user("hello")]),
                Box::new(|_input: AgentInput| -> EventStream {
                    Box::pin(futures::stream::empty())
                }),
            );
            assert!(drain(stream).await.is_empty());
            let decision = middleware.decision(&ctx).unwrap();
            assert!(
                decision.model.is_none(),
                "expected a fallback: {decision:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_transport_failure_falls_back_or_errors_like_the_java_builder() {
        let fail_open = ModelRouterMiddleware::builder(failing("backend down"))
            .choice("fast", model("fast"), "cheap")
            .choice("slow", model("slow"), "thorough")
            .enabled(true)
            .build()
            .unwrap();
        let ctx = RuntimeContext::new().with_session_id("s-open");
        let stream = fail_open.on_agent(
            &ctx,
            AgentInput::new(vec![Msg::user("hello")]),
            Box::new(|_input: AgentInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        assert!(first_error(stream).await.is_none());
        assert!(fail_open.decision(&ctx).unwrap().is_fallback());

        let fail_closed = ModelRouterMiddleware::builder(failing("backend down"))
            .choice("fast", model("fast"), "cheap")
            .choice("slow", model("slow"), "thorough")
            .enabled(true)
            .fail_open(false)
            .build()
            .unwrap();
        let ctx = RuntimeContext::new().with_session_id("s-closed");
        let stream = fail_closed.on_agent(
            &ctx,
            AgentInput::new(vec![Msg::user("hello")]),
            Box::new(|_input: AgentInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        assert_eq!(
            first_error(stream).await.as_deref(),
            Some("Agent error: backend down")
        );
    }

    #[tokio::test]
    async fn the_router_requires_explicit_compatibility_for_tool_calls() {
        let middleware = ModelRouterMiddleware::builder(answering(choice_reply(
            "slow",
            0.9,
            &[("fast", 0.1), ("slow", 0.9)],
        )))
        .choice("fast", model("fast"), "cheap")
        .choice("slow", model("slow"), "thorough")
        .enabled(true)
        .build()
        .unwrap();
        let ctx = RuntimeContext::new().with_session_id("s-tools");
        let stream = middleware.on_agent(
            &ctx,
            AgentInput::new(vec![Msg::user("use tools")]),
            Box::new(|_input: AgentInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        drain(stream).await;

        let called = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&called);
        let stream = middleware.on_model_call(
            &ctx,
            ModelCallInput::new(
                vec![Msg::user("hi")],
                vec![ToolSchema::new("search", "search the web")],
                GenerateOptions::default(),
                model("fast"),
            ),
            Box::new(move |input: ModelCallInput| -> EventStream {
                sink.lock()
                    .unwrap()
                    .push(input.model.model_name().to_string());
                Box::pin(futures::stream::empty())
            }),
        );
        drain(stream).await;
        assert_eq!(*called.lock().unwrap(), vec!["fast".to_string()]);
        // The rejected route is dropped, so the next call is not attempted again.
        assert!(middleware.decision(&ctx).unwrap().is_fallback());
    }

    #[tokio::test]
    async fn a_reused_context_does_not_inherit_a_previous_route() {
        let router = ModelRouterMiddleware::builder(answering(choice_reply(
            "slow",
            0.9,
            &[("fast", 0.1), ("slow", 0.9)],
        )))
        .choice("fast", model("fast"), "cheap")
        .choice("slow", model("slow"), "thorough")
        .enabled(true)
        .build()
        .unwrap();
        let ctx = RuntimeContext::new().with_session_id("s-reuse");
        let stream = router.on_agent(
            &ctx,
            AgentInput::new(vec![Msg::user("route me")]),
            Box::new(|_input: AgentInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        drain(stream).await;
        assert_eq!(router.decision(&ctx).unwrap().model_name(), Some("slow"));

        // A second invocation without user text must clear the route.
        let stream = router.on_agent(
            &ctx,
            AgentInput::new(Vec::new()),
            Box::new(|_input: AgentInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        drain(stream).await;
        assert!(router.decision(&ctx).unwrap().is_fallback());
    }

    #[tokio::test]
    async fn routing_configuration_is_validated() {
        let single = ModelRouterMiddleware::builder(answering(predicate_reply(&[])))
            .choice("fast", model("fast"), "cheap")
            .build();
        assert_eq!(
            single.err().unwrap().message(),
            "require 2\u{2013}255 model choices"
        );

        let blank = ModelRouterMiddleware::builder(answering(predicate_reply(&[])))
            .choice("fast", model("fast"), "cheap")
            .choice("slow", model("slow"), "  ")
            .build();
        assert_eq!(blank.err().unwrap().message(), "criteria must not be blank");

        let threshold = ModelRouterMiddleware::builder(answering(predicate_reply(&[])))
            .choice("fast", model("fast"), "cheap")
            .choice("slow", model("slow"), "thorough")
            .confidence_threshold(1.5)
            .build();
        assert_eq!(
            threshold.err().unwrap().message(),
            "confidenceThreshold must be finite and between 0 and 1"
        );
    }

    // -----------------------------------------------------------------------------------------
    // Tool selection
    // -----------------------------------------------------------------------------------------

    fn reasoning_input(names: &[&str]) -> ReasoningInput {
        ReasoningInput::new(
            vec![Msg::user("find the numbers")],
            names
                .iter()
                .map(|name| ToolSchema::new(*name, format!("the {name} tool")))
                .collect(),
            GenerateOptions::default(),
        )
    }

    fn selection_probe() -> (Arc<Mutex<Vec<Vec<String>>>>, Arc<Mutex<usize>>) {
        (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(0)))
    }

    fn selection_next(
        tools: Arc<Mutex<Vec<Vec<String>>>>,
        calls: Arc<Mutex<usize>>,
    ) -> Next<ReasoningInput> {
        Box::new(move |input: ReasoningInput| -> EventStream {
            *calls.lock().unwrap() += 1;
            tools
                .lock()
                .unwrap()
                .push(input.tools.iter().map(|tool| tool.name.clone()).collect());
            Box::pin(futures::stream::empty())
        })
    }

    #[tokio::test]
    async fn selection_keeps_essential_tools_caps_relevant_ones_and_preserves_order() {
        let middleware = ToolSelectionMiddleware::builder(answering(predicate_reply(&[
            ("tool_0", 0.95),
            ("tool_1", 0.9),
            // Below the rejection threshold: an ambiguous band (0.2..0.8) would leave the
            // tool set untouched by design, so the third tool is clearly irrelevant.
            ("tool_2", 0.1),
        ])))
        .enabled(true)
        .max_tools(2)
        .build()
        .unwrap();
        let (tools, calls) = selection_probe();
        let stream = middleware.on_reasoning(
            &RuntimeContext::new(),
            reasoning_input(&["search", "sql", "email", "generate_response"]),
            selection_next(Arc::clone(&tools), Arc::clone(&calls)),
        );
        drain(stream).await;
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(
            tools.lock().unwrap()[0],
            vec![
                "search".to_string(),
                "sql".to_string(),
                "generate_response".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn selection_can_remove_every_optional_tool_and_delegates_once() {
        let middleware = ToolSelectionMiddleware::builder(answering(predicate_reply(&[
            ("tool_0", 0.05),
            ("tool_1", 0.0),
        ])))
        .enabled(true)
        .build()
        .unwrap();
        let (tools, calls) = selection_probe();
        let stream = middleware.on_reasoning(
            &RuntimeContext::new(),
            reasoning_input(&["search", "sql", "generate_response"]),
            selection_next(Arc::clone(&tools), Arc::clone(&calls)),
        );
        drain(stream).await;
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(
            tools.lock().unwrap()[0],
            vec!["generate_response".to_string()]
        );
    }

    #[tokio::test]
    async fn selection_retains_the_original_tools_on_ambiguity_refusal_or_failure() {
        let cases: Vec<DecisionCaller> = vec![
            answering(predicate_reply(&[("tool_0", 0.5), ("tool_1", 0.9)])),
            answering(refusal_reply("tool_0")),
            failing("selection backend down"),
        ];
        for caller in cases {
            let middleware = ToolSelectionMiddleware::builder(caller)
                .enabled(true)
                .build()
                .unwrap();
            let (tools, calls) = selection_probe();
            let stream = middleware.on_reasoning(
                &RuntimeContext::new(),
                reasoning_input(&["search", "sql", "generate_response"]),
                selection_next(Arc::clone(&tools), Arc::clone(&calls)),
            );
            assert!(first_error(stream).await.is_none());
            assert_eq!(*calls.lock().unwrap(), 1);
            assert_eq!(
                tools.lock().unwrap()[0],
                vec![
                    "search".to_string(),
                    "sql".to_string(),
                    "generate_response".to_string()
                ]
            );
        }
    }

    #[tokio::test]
    async fn selection_fails_closed_only_when_asked_to() {
        let middleware = ToolSelectionMiddleware::builder(failing("selection backend down"))
            .enabled(true)
            .fail_open(false)
            .build()
            .unwrap();
        let (tools, calls) = selection_probe();
        let stream = middleware.on_reasoning(
            &RuntimeContext::new(),
            reasoning_input(&["search", "generate_response"]),
            selection_next(Arc::clone(&tools), Arc::clone(&calls)),
        );
        assert_eq!(
            first_error(stream).await.as_deref(),
            Some("Agent error: selection backend down")
        );
        assert_eq!(*calls.lock().unwrap(), 0);
        assert!(tools.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn selection_batches_more_than_four_hundred_tools() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let caller: DecisionCaller = Arc::new(move |request: DecisionRequest| {
            let mut answers = BTreeMap::new();
            for name in request.questions().keys() {
                answers.insert(name.clone(), DecisionAnswer::Predicate { probability: 0.9 });
            }
            seen.lock().unwrap().push(request.questions().len());
            let reply: Result<DecisionResult, DecisionsError> = Ok(DecisionResult {
                model: "decisions-1".to_string(),
                answers,
                usage: None,
            });
            Box::pin(async move { reply })
        });
        let middleware = ToolSelectionMiddleware::builder(caller)
            .enabled(true)
            .build()
            .unwrap();
        let names: Vec<String> = (0..401).map(|index| format!("tool-{index}")).collect();
        let tools: Vec<ToolSchema> = names
            .iter()
            .map(|name| ToolSchema::new(name.clone(), "optional"))
            .collect();
        let (kept, calls) = selection_probe();
        let stream = middleware.on_reasoning(
            &RuntimeContext::new(),
            ReasoningInput::new(
                vec![Msg::user("pick tools")],
                tools,
                GenerateOptions::default(),
            ),
            selection_next(Arc::clone(&kept), Arc::clone(&calls)),
        );
        drain(stream).await;
        assert_eq!(*requests.lock().unwrap(), vec![400, 1]);
        assert_eq!(*calls.lock().unwrap(), 1);
        // Three relevant optional tools survive (default maxTools).
        assert_eq!(kept.lock().unwrap()[0].len(), DEFAULT_MAX_TOOLS);
    }

    #[tokio::test]
    async fn a_forced_tool_is_never_judged() {
        let middleware =
            ToolSelectionMiddleware::builder(answering(predicate_reply(&[("tool_0", 0.1)])))
                .enabled(true)
                .build()
                .unwrap();
        let mut input = reasoning_input(&["search", "delete_all", "generate_response"]);
        input.options.tool_choice = Some(ToolChoice::Specific {
            tool_name: "delete_all".to_string(),
        });
        let (tools, calls) = selection_probe();
        let stream = middleware.on_reasoning(
            &RuntimeContext::new(),
            input,
            selection_next(Arc::clone(&tools), Arc::clone(&calls)),
        );
        drain(stream).await;
        assert_eq!(tools.lock().unwrap()[0].len(), 2);
        assert!(tools.lock().unwrap()[0].contains(&"delete_all".to_string()));
        assert!(!tools.lock().unwrap()[0].contains(&"search".to_string()));
    }

    #[tokio::test]
    async fn selection_uses_tool_result_evidence() {
        let evidences = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&evidences);
        let caller: DecisionCaller = Arc::new(move |request: DecisionRequest| {
            seen.lock()
                .unwrap()
                .push(request.input().text_value().unwrap_or_default().to_string());
            let reply: Result<DecisionResult, DecisionsError> =
                Ok(predicate_reply(&[("tool_0", 0.95)]));
            Box::pin(async move { reply })
        });
        let middleware = ToolSelectionMiddleware::builder(caller)
            .enabled(true)
            .build()
            .unwrap();
        let input = ReasoningInput::new(
            vec![
                Msg::user("hello"),
                Msg::tool("c1", "lookup", "fetched page"),
            ],
            vec![ToolSchema::new("search", "search the web")],
            GenerateOptions::default(),
        );
        let (tools, calls) = selection_probe();
        let stream = middleware.on_reasoning(
            &RuntimeContext::new(),
            input,
            selection_next(Arc::clone(&tools), Arc::clone(&calls)),
        );
        drain(stream).await;
        let evidence = evidences.lock().unwrap()[0].clone();
        assert!(evidence.contains("fetched page"), "{evidence}");
        assert!(evidence.contains("hello"), "{evidence}");
    }

    #[tokio::test]
    async fn selection_configuration_is_validated() {
        let thresholds = ToolSelectionMiddleware::builder(answering(predicate_reply(&[])))
            .rejection_threshold(0.9)
            .confidence_threshold(0.8)
            .build();
        assert_eq!(
            thresholds.err().unwrap().message(),
            "require positive limits, nonblank tool names and rejection < selection"
        );
        let limits = ToolSelectionMiddleware::builder(answering(predicate_reply(&[])))
            .max_tools(0)
            .build();
        assert_eq!(
            limits.err().unwrap().message(),
            "require positive limits, nonblank tool names and rejection < selection"
        );
        let blank = ToolSelectionMiddleware::builder(answering(predicate_reply(&[])))
            .always_include_tools([" "])
            .build();
        assert_eq!(
            blank.err().unwrap().message(),
            "require positive limits, nonblank tool names and rejection < selection"
        );
    }

    // -----------------------------------------------------------------------------------------
    // Guarded tool calls
    // -----------------------------------------------------------------------------------------

    fn stateful_context() -> RuntimeContext {
        let mut ctx = RuntimeContext::new();
        ctx.state = Some(Arc::new(Mutex::new(AgentState::default())));
        ctx
    }

    fn guard_next(passing: Arc<Mutex<Vec<Vec<String>>>>) -> Next<ActingInput> {
        Box::new(move |input: ActingInput| -> EventStream {
            passing.lock().unwrap().push(
                input
                    .tool_calls
                    .iter()
                    .map(|call| call.id.clone())
                    .collect(),
            );
            Box::pin(futures::stream::iter(vec![Ok(AgentEvent::with_fields(
                AgentEventType::AgentEnd,
                JsonMap::new(),
            ))]))
        })
    }

    #[tokio::test]
    async fn the_guard_denies_risky_and_refused_calls_and_keeps_the_others() {
        let middleware = AutoModeMiddleware::builder(answering(predicate_reply(&[
            ("tool_0", 0.95),
            ("tool_1", 0.1),
        ])))
        .guarded_tools(["drop_table", "delete_file"])
        .enabled(true)
        .build()
        .unwrap();
        let input = ActingInput::new(vec![
            tool_call("c1", "drop_table"),
            tool_call("c2", "delete_file"),
            tool_call("c3", "read_file"),
        ]);
        let results = input.results.clone();
        let passing = Arc::new(Mutex::new(Vec::new()));
        let stream =
            middleware.on_acting(&stateful_context(), input, guard_next(Arc::clone(&passing)));
        let events = drain(stream).await;

        assert_eq!(
            *passing.lock().unwrap(),
            vec![vec!["c1".to_string(), "c3".to_string()]]
        );
        let ends: Vec<&AgentEvent> = events
            .iter()
            .filter(|event| event.event_type() == AgentEventType::ToolResultEnd)
            .collect();
        assert_eq!(ends.len(), 1);
        assert_eq!(
            ends[0].data().fields.get("tool_call_id"),
            Some(&json!("c2"))
        );
        assert_eq!(ends[0].data().fields.get("state"), Some(&json!("DENIED")));
        assert_eq!(
            events.last().map(AgentEvent::event_type),
            Some(AgentEventType::AgentEnd),
            "the delegated events follow the denials"
        );
        let snapshot = results.snapshot();
        assert_eq!(snapshot.results.len(), 1);
        assert_eq!(snapshot.results[0].state, ToolResultState::Denied);
        assert!(matches!(
            snapshot.results[0].output.first(),
            Some(ContentBlock::Text(text)) if text.text == DEFAULT_DENY_MESSAGE
        ));
    }

    #[tokio::test]
    async fn the_guard_fails_closed_without_a_state_or_on_an_error() {
        // No AgentState: Java denies every guarded call when `failOpen` is false.
        let middleware =
            AutoModeMiddleware::builder(answering(predicate_reply(&[("tool_0", 0.99)])))
                .guarded_tool("drop_table")
                .enabled(true)
                .build()
                .unwrap();
        let input = ActingInput::new(vec![tool_call("c1", "drop_table")]);
        let results = input.results.clone();
        let passing = Arc::new(Mutex::new(Vec::new()));
        let stream = middleware.on_acting(
            &RuntimeContext::new(),
            input,
            guard_next(Arc::clone(&passing)),
        );
        drain(stream).await;
        assert!(passing.lock().unwrap().is_empty());
        assert_eq!(results.snapshot().results.len(), 1);

        // A failed assessment denies by default as well.
        let failing_guard = AutoModeMiddleware::builder(failing("guard backend down"))
            .guarded_tool("drop_table")
            .enabled(true)
            .build()
            .unwrap();
        let input = ActingInput::new(vec![tool_call("c1", "drop_table")]);
        let results = input.results.clone();
        let passing = Arc::new(Mutex::new(Vec::new()));
        let stream =
            failing_guard.on_acting(&stateful_context(), input, guard_next(Arc::clone(&passing)));
        drain(stream).await;
        assert!(passing.lock().unwrap().is_empty());
        assert_eq!(results.snapshot().results.len(), 1);
    }

    #[tokio::test]
    async fn explicit_fail_open_allows_a_failed_assessment() {
        let middleware = AutoModeMiddleware::builder(failing("guard backend down"))
            .guarded_tool("drop_table")
            .enabled(true)
            .fail_open(true)
            .build()
            .unwrap();
        let input = ActingInput::new(vec![
            tool_call("c1", "drop_table"),
            tool_call("c2", "read_file"),
        ]);
        let results = input.results.clone();
        let passing = Arc::new(Mutex::new(Vec::new()));
        let stream =
            middleware.on_acting(&stateful_context(), input, guard_next(Arc::clone(&passing)));
        drain(stream).await;
        assert_eq!(
            *passing.lock().unwrap(),
            vec![vec!["c1".to_string(), "c2".to_string()]]
        );
        assert!(results.snapshot().results.is_empty());
    }

    #[tokio::test]
    async fn the_guard_batches_more_than_four_hundred_calls() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let caller: DecisionCaller = Arc::new(move |request: DecisionRequest| {
            let mut answers = BTreeMap::new();
            for name in request.questions().keys() {
                answers.insert(
                    name.clone(),
                    DecisionAnswer::Predicate { probability: 0.95 },
                );
            }
            seen.lock().unwrap().push(request.questions().len());
            let reply: Result<DecisionResult, DecisionsError> = Ok(DecisionResult {
                model: "decisions-1".to_string(),
                answers,
                usage: None,
            });
            Box::pin(async move { reply })
        });
        let middleware = AutoModeMiddleware::builder(caller)
            .guarded_tool("run")
            .enabled(true)
            .build()
            .unwrap();
        let calls: Vec<ToolUseBlock> = (0..401)
            .map(|index| tool_call(&format!("c{index}"), "run"))
            .collect();
        let input = ActingInput::new(calls);
        let results = input.results.clone();
        let passing = Arc::new(Mutex::new(Vec::new()));
        let stream =
            middleware.on_acting(&stateful_context(), input, guard_next(Arc::clone(&passing)));
        drain(stream).await;
        assert_eq!(*requests.lock().unwrap(), vec![400, 1]);
        assert_eq!(results.snapshot().results.len(), 0);
        assert_eq!(passing.lock().unwrap()[0].len(), 401);
    }

    #[tokio::test]
    async fn the_guard_ignores_allowed_and_unguarded_calls() {
        let middleware =
            AutoModeMiddleware::builder(answering(predicate_reply(&[("tool_0", 0.0)])))
                .guarded_tool("drop_table")
                .enabled(true)
                .build()
                .unwrap();
        let mut allowed = tool_call("c1", "drop_table");
        allowed.state = ToolCallState::Allowed;
        let input = ActingInput::new(vec![allowed, tool_call("c2", "read_file")]);
        let results = input.results.clone();
        let passing = Arc::new(Mutex::new(Vec::new()));
        let stream =
            middleware.on_acting(&stateful_context(), input, guard_next(Arc::clone(&passing)));
        let events = drain(stream).await;
        assert_eq!(
            *passing.lock().unwrap(),
            vec![vec!["c1".to_string(), "c2".to_string()]]
        );
        assert!(results.snapshot().results.is_empty());
        assert!(
            !events
                .iter()
                .any(|event| event.event_type() == AgentEventType::ToolResultEnd),
            "no denial events for an unjudged batch"
        );
    }

    #[tokio::test]
    async fn guard_configuration_is_validated() {
        let empty = AutoModeMiddleware::builder(answering(predicate_reply(&[])))
            .enabled(true)
            .build();
        assert_eq!(
            empty.err().unwrap().message(),
            "guardedTools and denyMessage must not be empty"
        );
        let threshold = AutoModeMiddleware::builder(answering(predicate_reply(&[])))
            .guarded_tool("drop_table")
            .safety_threshold(-0.5)
            .build();
        assert_eq!(
            threshold.err().unwrap().message(),
            "safetyThreshold must be finite and between 0 and 1"
        );
    }

    #[tokio::test]
    async fn every_middleware_is_disabled_until_enabled() {
        let router = ModelRouterMiddleware::builder(answering(choice_reply(
            "slow",
            0.9,
            &[("fast", 0.1), ("slow", 0.9)],
        )))
        .choice("fast", model("fast"), "cheap")
        .choice("slow", model("slow"), "thorough")
        .build()
        .unwrap();
        let ctx = RuntimeContext::new().with_session_id("s-disabled");
        let stream = router.on_agent(
            &ctx,
            AgentInput::new(vec![Msg::user("hi")]),
            Box::new(|_input: AgentInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        assert!(drain(stream).await.is_empty());
        assert!(router.decision(&ctx).unwrap().is_fallback());

        let selection =
            ToolSelectionMiddleware::builder(answering(predicate_reply(&[("tool_0", 0.95)])))
                .build()
                .unwrap();
        let (tools, calls) = selection_probe();
        let stream = selection.on_reasoning(
            &RuntimeContext::new(),
            reasoning_input(&["search", "generate_response"]),
            selection_next(Arc::clone(&tools), Arc::clone(&calls)),
        );
        drain(stream).await;
        assert_eq!(tools.lock().unwrap()[0].len(), 2);

        let guard = AutoModeMiddleware::builder(answering(predicate_reply(&[("tool_0", 0.0)])))
            .guarded_tool("drop_table")
            .build()
            .unwrap();
        let input = ActingInput::new(vec![tool_call("c1", "drop_table")]);
        let results = input.results.clone();
        let passing = Arc::new(Mutex::new(Vec::new()));
        let stream = guard.on_acting(
            &RuntimeContext::new(),
            input,
            guard_next(Arc::clone(&passing)),
        );
        drain(stream).await;
        assert_eq!(*passing.lock().unwrap(), vec![vec!["c1".to_string()]]);
        assert!(results.snapshot().results.is_empty());
    }
}
