//! The three JEV **selection** middlewares (Java `jev/example`, tracker `JUDGE-01`).
//!
//! Java keeps these classes in a package called `example`, but they are runtime behaviour: the
//! `JevStageRouter` holds `JevModelRouterMiddleware` as a field and a host binds all three like
//! any other `MiddlewareBase`. They are ported here next to the other JEV hooks, following the
//! structure of [`crate::openai_decisions::middleware`]:
//!
//! | Java | Rust | Hook |
//! |---|---|---|
//! | `JevModelRouterMiddleware` | [`JevModelRouterMiddleware`] | `ON_AGENT` + `ON_MODEL_CALL` |
//! | `JevToolSelectionMiddleware` | [`JevToolSelectionMiddleware`] | `ON_REASONING` |
//! | `JevAutoModeMiddleware` | [`JevAutoModeMiddleware`] | `ON_ACTING` |
//! | `JevSelectionSupport` | the free functions in this module | — |
//!
//! Every network call goes through the injected [`Caller`] seam (production:
//! [`caller_from_client`](crate::jev::call::caller_from_client)), runs under a
//! [`JevExecution`] budget, and therefore stays offline in tests. The acceptance rule is the same
//! for all three: `Mode::Off` is a pure pass-through, `Mode::Shadow` observes without changing
//! anything, only `Mode::Enforce` rewrites the request.
//!
//! Documented differences (tracker §4.3):
//!
//! * Java stores the routing decision in Reactor's `Context` and reads it back through the static
//!   `JevModelRouterMiddleware.decision(ctx)`. `&RuntimeContext` is immutable here, so the router
//!   owns a `run_id`/`session_id` keyed map ([`JevModelRouterMiddleware::decision`]), exactly like
//!   [`crate::openai_decisions::middleware::ModelRouterMiddleware`].
//! * Java's `phaseRouting` advanced mode (delegate both hooks to `JevPhaseRouting`) is not
//!   duplicated: hosts that resolve phases use [`super::PhaseRoutingMiddleware`] directly.
//! * A denied tool result is recorded through [`ActingInput::results`] instead of appending to
//!   `AgentState.context`; the core persists whatever the handle holds.
//! * The `tool` payload of the tool-selection questions is the Rust [`ToolSchema`] serialisation
//!   (camelCase), not Java's Jackson form; both describe the same schema.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::{Arc, Mutex};

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result as CoreResult;
use agentscope_core::event::{AgentEvent, AgentEventType};
use agentscope_core::message::{
    ContentBlock, Msg, MsgRole, TextBlock, ToolCallState, ToolResultBlock, ToolResultState,
    ToolUseBlock,
};
use agentscope_core::middleware::{
    ActingInput, AgentInput, EventStream, ExtensionPoint, Middleware, ModelCallInput, Next,
    ReasoningInput,
};
use agentscope_core::model::{Model, ToolSchema};
use futures::StreamExt;
use serde_json::{json, Value};

use crate::error::JevError;
use crate::jev::call::{caller_from_client, execute_guarded, Caller};
use crate::jev::client::JevClient;
use crate::jev::confirmed_calls::{self};
use crate::jev::dto::{
    Answer, ChoiceAnswer, ChoiceQuestion, NoulAnswer, NoulQuestion, Question, SystemOneRequest,
    SystemOneResult,
};
use crate::jev::execution::{Decision, JevExecution, Mode, Options, Status};
use crate::jev::runtime::middleware::run_key;

/// The option id meaning "none of these" (`JevSelectionSupport.NONE_OPTION`).
pub const NONE_OPTION: &str = "__none__";

/// The maximum number of closed-choice options a JEV question accepts
/// (`JevSelectionSupport.MAX_CHOICE_OPTIONS`).
pub const MAX_CHOICE_OPTIONS: usize = 255;

/// The maximum number of *named* candidates (options minus [`NONE_OPTION`]).
pub const MAX_CANDIDATES_PER_CHOICE: usize = MAX_CHOICE_OPTIONS - 1;

/// The question id the model router asks (`JevModelRouterMiddleware.ROUTING_QUESTION`).
pub const ROUTING_QUESTION: &str = "models_0";

/// Java `JevModelRouterMiddleware.DEFAULT_INSTRUCTIONS`.
pub const DEFAULT_ROUTING_INSTRUCTIONS: &str =
    "Choose the least costly model that can complete the task.";

/// Java `JevModelRouterMiddleware.Builder.confidenceThreshold` default.
pub const DEFAULT_ROUTING_CONFIDENCE: f64 = 0.8;

/// Java `JevToolSelectionMiddleware.Builder.alwaysIncludeTools` default.
pub const DEFAULT_ALWAYS_INCLUDE_TOOLS: [&str; 3] =
    ["load_skill_through_path", "reset_tools", "generate_response"];

/// Java `JevToolSelectionMiddleware.Builder.maxTools` default.
pub const DEFAULT_MAX_TOOLS: usize = 3;

/// Java `JevToolSelectionMiddleware.Builder.confidenceThreshold` default.
pub const DEFAULT_SELECTION_CONFIDENCE: f64 = 0.8;

/// Java `JevToolSelectionMiddleware.Builder.rejectionThreshold` default.
pub const DEFAULT_REJECTION_THRESHOLD: f64 = 0.2;

/// Java `JevToolSelectionMiddleware.Builder.maxContextMessages` default.
pub const DEFAULT_MAX_CONTEXT_MESSAGES: usize = 8;

/// Java `JevToolSelectionMiddleware.Builder.maxContextChars` default.
pub const DEFAULT_MAX_CONTEXT_CHARS: usize = 8000;

/// Java `JevAutoModeMiddleware.Builder.safetyThreshold` default.
pub const DEFAULT_SAFETY_THRESHOLD: f64 = 0.8;

/// Java `JevAutoModeMiddleware.DEFAULT_DENY_MESSAGE` (verbatim).
pub const DEFAULT_DENY_MESSAGE: &str =
    "Blocked: this tool call did not pass the automatic-execution check. Request review or propose a safer alternative.";

/// The reply id used when the runtime context carries no agent state.
pub const GUARD_REPLY_ID: &str = "jev-guard";

/// The prefix of the per-call risk / relevance question ids (`tool_0`, `tool_1`, ...).
pub const QUESTION_ID_PREFIX: &str = "tool_";

/// How many tools one relevance request batch carries.
pub const TOOL_BATCH_SIZE: usize = 64;

// ---------------------------------------------------------------------------------------------
// JevSelectionSupport
// ---------------------------------------------------------------------------------------------

/// The latest USER turn's text, trimmed (`JevSelectionSupport.latestUserText`).
pub fn latest_user_text(messages: &[Msg]) -> String {
    for message in messages.iter().rev() {
        if message.role() == MsgRole::User {
            return message.text_content().trim().to_string();
        }
    }
    String::new()
}

/// The state payload for the `LATEST_USER_MESSAGE` strategy (`userRequestState`).
pub fn latest_user_state(user_text: &str) -> Value {
    json!({ "userRequest": user_text })
}

/// The state payload carrying the complete conversation (`messagesState`).
pub fn messages_state(messages: &[Msg]) -> Value {
    json!({ "messages": messages })
}

/// A bounded window of the newest non-blank messages (`recentWindowState`).
///
/// Each message is projected to `{role, text}`; blank ones are skipped and do not consume a
/// window slot. At most `max_messages` entries and `max_chars` characters are kept, counted from
/// the newest message backwards, and the oldest kept message is clipped when the budget runs out.
pub fn recent_window_state(messages: &[Msg], max_messages: usize, max_chars: usize) -> Value {
    if messages.is_empty() || max_messages == 0 || max_chars == 0 {
        return json!({ "messages": [] });
    }
    let mut entries: Vec<Value> = Vec::new();
    let mut remaining = max_chars;
    for message in messages.iter().rev() {
        if remaining == 0 || entries.len() >= max_messages {
            break;
        }
        let text = message.text_content();
        if text.trim().is_empty() {
            continue;
        }
        let text = if text.chars().count() > remaining {
            let clipped: String = text.chars().take(remaining).collect();
            if clipped.trim().is_empty() {
                continue;
            }
            clipped
        } else {
            text
        };
        remaining -= text.chars().count();
        entries.insert(0, json!({ "role": role_name(message.role()), "text": text }));
    }
    json!({ "messages": entries })
}

/// The `{option: description}` criteria of a tool question, ending with [`NONE_OPTION`]
/// (`toolCriteria`).
pub fn tool_criteria(tools: &[ToolSchema]) -> serde_json::Map<String, Value> {
    let mut criteria = serde_json::Map::new();
    for tool in tools {
        criteria.insert(tool.name.clone(), Value::String(tool.description.clone()));
    }
    criteria.insert(
        NONE_OPTION.to_string(),
        Value::String("No additional tool is needed for this request.".to_string()),
    );
    criteria
}

/// Splits `values` into `size`-sized batches (`JevSelectionSupport.partition`).
pub fn partition<T: Clone>(values: &[T], size: usize) -> Vec<Vec<T>> {
    if values.is_empty() || size == 0 {
        return Vec::new();
    }
    values.chunks(size).map(|chunk| chunk.to_vec()).collect()
}

/// The option names a closed-choice answer selected (`selectedNames`).
///
/// Empty when the answer has no distribution, its confidence is missing or below
/// `confidence_threshold`, or nothing beats [`NONE_OPTION`]. Unlike Java's `LinkedHashMap`, the
/// Rust `serde_json` map is ordered, so equal probabilities tie-break by option id.
pub fn selected_names(
    answer: &ChoiceAnswer,
    limit: usize,
    confidence_threshold: f64,
) -> Vec<String> {
    let Some(probabilities) = answer.probabilities.as_ref() else {
        return Vec::new();
    };
    let Some(confidence) = answer.confidence else {
        return Vec::new();
    };
    if probabilities.is_empty() || confidence < confidence_threshold || limit == 0 {
        return Vec::new();
    }
    let none_probability = probabilities.get(NONE_OPTION).copied().unwrap_or(0.0);
    let mut entries: Vec<(String, f64)> = probabilities
        .iter()
        .filter(|(name, probability)| name.as_str() != NONE_OPTION && **probability > none_probability)
        .map(|(name, probability)| (name.clone(), *probability))
        .collect();
    entries.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    entries
        .into_iter()
        .take(limit)
        .map(|(name, _)| name)
        .collect()
}

/// The single best option name (`topName`), or `None` when no option beats [`NONE_OPTION`].
pub fn top_name(answer: &ChoiceAnswer) -> Option<String> {
    let probabilities = answer.probabilities.as_ref()?;
    if probabilities.is_empty() {
        return None;
    }
    let none_probability = probabilities.get(NONE_OPTION).copied().unwrap_or(0.0);
    probabilities
        .iter()
        .filter(|(name, probability)| {
            name.as_str() != NONE_OPTION && **probability > none_probability
        })
        .max_by(|left, right| {
            left.1
                .partial_cmp(right.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(name, _)| name.clone())
}

/// The SDK name of a role, lowercased (`getRole().name().toLowerCase()`).
fn role_name(role: MsgRole) -> &'static str {
    match role {
        MsgRole::User => "user",
        MsgRole::Assistant => "assistant",
        MsgRole::System => "system",
        MsgRole::Tool => "tool",
    }
}

// ---------------------------------------------------------------------------------------------
// 1. Model routing (`JevModelRouterMiddleware`)
// ---------------------------------------------------------------------------------------------

/// One selectable model plus the criteria JEV judges it by (`JevModelRouterMiddleware.ModelChoice`).
#[derive(Clone)]
pub struct ModelChoice {
    /// The model this choice resolves to.
    pub model: Arc<dyn Model>,
    /// The routing criteria shown to JEV (must not be blank).
    pub criteria: String,
}

impl ModelChoice {
    /// Creates a choice (`ModelChoice` canonical constructor).
    pub fn new(model: Arc<dyn Model>, criteria: impl Into<String>) -> Result<Self, JevError> {
        let criteria = criteria.into();
        if criteria.trim().is_empty() {
            return Err(JevError::invalid("criteria must not be blank"));
        }
        Ok(Self { model, criteria })
    }

    /// The model's own name.
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

/// The selected model and the calibrated values JEV returned
/// (`JevModelRouterMiddleware.RoutingDecision`).
#[derive(Clone, Default)]
pub struct RoutingDecision {
    /// The chosen model, absent for a fallback decision.
    pub model: Option<Arc<dyn Model>>,
    /// Probability per candidate name.
    pub probabilities: BTreeMap<String, f64>,
    /// Model confidence in `[0, 1]`.
    pub confidence: Option<f64>,
}

impl RoutingDecision {
    /// The decision Java stores before asking anything (`RoutingDecision.fallback`).
    pub fn fallback() -> Self {
        Self::default()
    }

    /// Whether no model was routed.
    pub fn is_fallback(&self) -> bool {
        self.model.is_none()
    }

    /// The routed model's name, when a model was routed.
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

/// Filters the candidate list before a request is built (`Builder.eligible`).
pub type EligibleFilter = Arc<dyn Fn(&RuntimeContext, &ModelChoice) -> bool + Send + Sync>;

/// Decides whether a routed model still fits the call (`Builder.compatible`).
pub type CompatibleFilter = Arc<dyn Fn(&ModelCallInput, &ModelChoice) -> bool + Send + Sync>;

/// Routes the agent to one of the configured models (`JevModelRouterMiddleware`).
///
/// One decision per invocation is taken in `on_agent` (using the latest user turn) and applied on
/// every subsequent `on_model_call`, so an intermediate tool result cannot switch models mid-run.
#[derive(Clone)]
pub struct JevModelRouterMiddleware {
    call: Caller,
    choices: Vec<(String, ModelChoice)>,
    instructions: String,
    confidence_threshold: f64,
    execution: JevExecution,
    eligible: EligibleFilter,
    compatible: CompatibleFilter,
    decisions: Arc<Mutex<HashMap<String, RoutingDecision>>>,
}

impl std::fmt::Debug for JevModelRouterMiddleware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JevModelRouterMiddleware")
            .field("choices", &self.choices.len())
            .finish_non_exhaustive()
    }
}

impl JevModelRouterMiddleware {
    /// Starts a builder over an injected caller.
    pub fn builder(call: Caller) -> JevModelRouterMiddlewareBuilder {
        JevModelRouterMiddlewareBuilder::new(call)
    }

    /// Starts a builder over the real JEV client.
    pub fn from_client(client: JevClient) -> JevModelRouterMiddlewareBuilder {
        JevModelRouterMiddlewareBuilder::new(caller_from_client(client))
    }

    /// The configured execution mode.
    pub fn mode(&self) -> Mode {
        self.execution.mode()
    }

    /// The routing confidence threshold.
    pub fn confidence_threshold(&self) -> f64 {
        self.confidence_threshold
    }

    /// The configured choices, in insertion order.
    pub fn choices(&self) -> Vec<&str> {
        self.choices.iter().map(|(name, _)| name.as_str()).collect()
    }

    /// The most recent decision for `ctx` (`JevModelRouterMiddleware.decision`).
    pub fn decision(&self, ctx: &RuntimeContext) -> Option<RoutingDecision> {
        self.decision_for_key(&run_key(ctx))
    }

    /// The most recent decision for an explicit run key.
    pub fn decision_for_key(&self, key: &str) -> Option<RoutingDecision> {
        self.decisions.lock().unwrap().get(key).cloned()
    }

    fn store(&self, key: &str, decision: RoutingDecision) {
        self.decisions
            .lock()
            .unwrap()
            .insert(key.to_string(), decision);
    }

    fn request(
        &self,
        user_text: &str,
        candidates: &[(String, ModelChoice)],
    ) -> SystemOneRequest {
        let mut criteria = serde_json::Map::new();
        for (name, choice) in candidates {
            criteria.insert(name.clone(), Value::String(choice.criteria.clone()));
        }
        SystemOneRequest::builder()
            .state(latest_user_state(user_text))
            .question(
                ROUTING_QUESTION,
                Question::Choice(ChoiceQuestion::new(self.instructions.clone(), criteria)),
            )
            .build()
    }

    /// Maps a JEV reply onto a routing decision (`selectedModel`).
    fn route(
        &self,
        result: &SystemOneResult,
        candidates: &[(String, ModelChoice)],
    ) -> Decision<RoutingDecision> {
        let answer = result
            .answers
            .as_ref()
            .and_then(|answers| answers.get(ROUTING_QUESTION));
        let Some(Answer::Choice(answer)) = answer else {
            return Decision::uncertain("UNUSABLE_ROUTE");
        };
        let Some(confidence) = answer.confidence else {
            return Decision::uncertain("UNUSABLE_ROUTE");
        };
        if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
            return Decision::uncertain("UNUSABLE_ROUTE");
        }
        let Some(selected) = answer.choice.as_deref() else {
            return Decision::uncertain("UNUSABLE_ROUTE");
        };
        let Some((_name, choice)) = candidates.iter().find(|(name, _)| name == selected) else {
            return Decision::uncertain("UNUSABLE_ROUTE");
        };
        if confidence < self.confidence_threshold {
            return Decision::uncertain("UNUSABLE_ROUTE");
        }
        let mut recommendation = BTreeMap::new();
        recommendation.insert("model".to_string(), choice.model.model_name().to_string());
        Decision::new(
            Status::Decided,
            Some(RoutingDecision {
                model: Some(Arc::clone(&choice.model)),
                probabilities: answer.probabilities.clone().unwrap_or_default(),
                confidence: Some(confidence),
            }),
            "ROUTED",
            recommendation,
        )
    }
}

impl Middleware for JevModelRouterMiddleware {
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
        let user_text = latest_user_text(&input.messages);
        if self.execution.mode() == Mode::Off || user_text.is_empty() {
            return next(input);
        }
        let candidates: Vec<(String, ModelChoice)> = self
            .choices
            .iter()
            .filter(|(_, choice)| (self.eligible)(ctx, choice))
            .cloned()
            .collect();
        if candidates.is_empty() {
            return next(input);
        }
        let request = self.request(&user_text, &candidates);
        let this = self.clone();
        deferred(async move {
            let runner = this.clone();
            let decision = execute_guarded(&this.execution, async move {
                let result = (runner.call)(request)
                    .await
                    .map_err(|failure| JevError::invalid(failure.message()))?;
                Ok(runner.route(&result, &candidates))
            })
            .await;
            if this.execution.mode() == Mode::Enforce && decision.status == Status::Decided {
                if let Some(routed) = decision.value.clone() {
                    this.store(&key, routed);
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
        if self.execution.mode() == Mode::Off {
            return next(input);
        }
        let key = run_key(ctx);
        let Some(decision) = self.decision_for_key(&key) else {
            return next(input);
        };
        let Some(model) = decision.model.clone() else {
            return next(input);
        };
        let Some(choice) = self
            .choices
            .iter()
            .find(|(_, choice)| Arc::ptr_eq(&choice.model, &model))
            .map(|(_, choice)| choice.clone())
        else {
            self.store(&key, RoutingDecision::fallback());
            return next(input);
        };
        if !(self.eligible)(ctx, &choice) || !(self.compatible)(&input, &choice) {
            self.store(&key, RoutingDecision::fallback());
            return next(input);
        }
        next(ModelCallInput { model, ..input })
    }
}

/// Incremental builder for [`JevModelRouterMiddleware`] (`JevModelRouterMiddleware.Builder`).
pub struct JevModelRouterMiddlewareBuilder {
    call: Caller,
    choices: Vec<(String, ModelChoice)>,
    instructions: String,
    confidence_threshold: f64,
    options: Options,
    eligible: Option<EligibleFilter>,
    compatible: Option<CompatibleFilter>,
}

impl JevModelRouterMiddlewareBuilder {
    /// Creates a builder with Java's defaults (disabled, threshold `0.8`).
    pub fn new(call: Caller) -> Self {
        Self {
            call,
            choices: Vec::new(),
            instructions: DEFAULT_ROUTING_INSTRUCTIONS.to_string(),
            confidence_threshold: DEFAULT_ROUTING_CONFIDENCE,
            options: Options::disabled(),
            eligible: None,
            compatible: None,
        }
    }

    /// Adds (or replaces) one named choice (`Builder.choice`).
    pub fn choice(mut self, name: impl Into<String>, model: Arc<dyn Model>, criteria: impl Into<String>) -> Self {
        let name = name.into();
        let criteria = criteria.into();
        match self.choices.iter_mut().find(|(key, _)| key == &name) {
            Some(slot) => slot.1 = ModelChoice { model, criteria },
            None => self.choices.push((name, ModelChoice { model, criteria })),
        }
        self
    }

    /// Replaces every choice (`Builder.choices`).
    pub fn choices(
        mut self,
        choices: impl IntoIterator<Item = (String, Arc<dyn Model>, String)>,
    ) -> Self {
        self.choices.clear();
        for (name, model, criteria) in choices {
            self.choices.push((name, ModelChoice { model, criteria }));
        }
        self
    }

    /// Sets the instructions (`Builder.instructions`).
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }

    /// Sets the confidence threshold (`Builder.confidenceThreshold`).
    pub fn confidence_threshold(mut self, threshold: f64) -> Self {
        self.confidence_threshold = threshold;
        self
    }

    /// Sets the execution mode/budget (`Builder.execution`).
    pub fn execution(mut self, options: Options) -> Self {
        self.options = options;
        self
    }

    /// Overrides the candidate filter (`Builder.eligible`).
    pub fn eligible(mut self, eligible: EligibleFilter) -> Self {
        self.eligible = Some(eligible);
        self
    }

    /// Overrides the request-compatibility filter (`Builder.compatible`).
    pub fn compatible(mut self, compatible: CompatibleFilter) -> Self {
        self.compatible = Some(compatible);
        self
    }

    /// Validates and builds (`JevModelRouterMiddleware.validate`).
    pub fn build(self) -> Result<JevModelRouterMiddleware, JevError> {
        if self.choices.is_empty() {
            return Err(JevError::invalid("at least one model choice are required"));
        }
        if self.choices.len() > MAX_CHOICE_OPTIONS {
            return Err(JevError::invalid(format!(
                "model choices must not exceed {MAX_CHOICE_OPTIONS}"
            )));
        }
        if self.choices.iter().any(|(name, _)| name.trim().is_empty()) {
            return Err(JevError::invalid("choice names must not be blank"));
        }
        if self
            .choices
            .iter()
            .any(|(_, choice)| choice.criteria.trim().is_empty())
        {
            return Err(JevError::invalid("criteria must not be blank"));
        }
        if self.instructions.trim().is_empty() {
            return Err(JevError::invalid("instructions must not be blank"));
        }
        if !self.confidence_threshold.is_finite() || !(0.0..=1.0).contains(&self.confidence_threshold)
        {
            return Err(JevError::invalid(
                "confidenceThreshold must be between 0 and 1",
            ));
        }
        let eligible: EligibleFilter = self.eligible.unwrap_or_else(|| Arc::new(|_, _| true));
        // Java's default compatibility predicate accepts a call without tools.
        let compatible: CompatibleFilter = self
            .compatible
            .unwrap_or_else(|| Arc::new(|input, _| input.tools.is_empty()));
        Ok(JevModelRouterMiddleware {
            call: self.call,
            choices: self.choices,
            instructions: self.instructions,
            confidence_threshold: self.confidence_threshold,
            execution: JevExecution::new("model-routing", self.options),
            eligible,
            compatible,
            decisions: Arc::new(Mutex::new(HashMap::new())),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// 2. Tool selection (`JevToolSelectionMiddleware`)
// ---------------------------------------------------------------------------------------------

/// How the conversation state sent to JEV is built (`ContextStrategy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextStrategy {
    /// Recent non-blank messages, bounded by count and characters (default).
    #[default]
    RecentWindow,
    /// Only the latest user text, as `userRequest`.
    LatestUserMessage,
    /// The complete message list.
    FullConversation,
}

/// Scores every optional tool independently and keeps the applicable ones
/// (`JevToolSelectionMiddleware`).
#[derive(Clone)]
pub struct JevToolSelectionMiddleware {
    call: Caller,
    always_include: Vec<String>,
    max_tools: usize,
    confidence_threshold: f64,
    rejection_threshold: f64,
    strategy: ContextStrategy,
    max_context_messages: usize,
    max_context_chars: usize,
    execution: JevExecution,
}

impl std::fmt::Debug for JevToolSelectionMiddleware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JevToolSelectionMiddleware")
            .field("max_tools", &self.max_tools)
            .finish_non_exhaustive()
    }
}

impl JevToolSelectionMiddleware {
    /// Starts a builder over an injected caller.
    pub fn builder(call: Caller) -> JevToolSelectionMiddlewareBuilder {
        JevToolSelectionMiddlewareBuilder::new(call)
    }

    /// Starts a builder over the real JEV client.
    pub fn from_client(client: JevClient) -> JevToolSelectionMiddlewareBuilder {
        JevToolSelectionMiddlewareBuilder::new(caller_from_client(client))
    }

    /// The configured execution mode.
    pub fn mode(&self) -> Mode {
        self.execution.mode()
    }

    /// The selection cap.
    pub fn max_tools(&self) -> usize {
        self.max_tools
    }

    /// The configured context strategy.
    pub fn context_strategy(&self) -> ContextStrategy {
        self.strategy
    }

    fn state_for(&self, messages: &[Msg]) -> Value {
        match self.strategy {
            ContextStrategy::RecentWindow => recent_window_state(
                messages,
                self.max_context_messages,
                self.max_context_chars,
            ),
            ContextStrategy::LatestUserMessage => latest_user_state(&latest_user_text(messages)),
            ContextStrategy::FullConversation => messages_state(messages),
        }
    }

    /// Asks JEV about every optional tool, in batches (`selectTools`).
    async fn select(
        &self,
        state: &Value,
        tools: &[ToolSchema],
    ) -> Result<Decision<Vec<String>>, JevError> {
        let mut scores: Vec<(String, f64)> = Vec::new();

        for batch in partition(tools, TOOL_BATCH_SIZE) {
            let mut builder = SystemOneRequest::builder().state(state.clone());
            for (index, tool) in batch.iter().enumerate() {
                builder = builder.question(
                    format!("{QUESTION_ID_PREFIX}{index}"),
                    Question::Noul(NoulQuestion::new(
                        json!({
                            "instruction": "Is this tool applicable to the current request? Judge independently of other tools.",
                            "tool": tool,
                        }),
                        None,
                    )),
                );
            }
            let result = (self.call)(builder.build())
                .await
                .map_err(|failure| JevError::invalid(failure.message()))?;
            let answers = result.answers.ok_or_else(|| JevError::invalid("invalid answers"))?;
            if answers.len() != batch.len() {
                return Err(JevError::invalid("invalid answers"));
            }
            for (index, tool) in batch.iter().enumerate() {
                let Some(Answer::Noul(answer)) = answers.get(&format!("{QUESTION_ID_PREFIX}{index}"))
                else {
                    return Err(JevError::invalid("invalid relevance"));
                };
                let Some(score) = answer.noul else {
                    return Err(JevError::invalid("invalid relevance"));
                };
                if !score.is_finite() || !(0.0..=1.0).contains(&score) {
                    return Err(JevError::invalid("invalid relevance"));
                }
                scores.push((tool.name.clone(), score));
            }
        }

        // One ambiguous score leaves the whole tool set untouched, so a half-scored set never
        // silently loses a useful tool.
        if scores.iter().any(|(_, score)| {
            *score > self.rejection_threshold && *score < self.confidence_threshold
        }) {
            return Ok(Decision::uncertain("UNCERTAIN_RELEVANCE"));
        }

        let mut qualifying: Vec<(String, f64)> = scores
            .into_iter()
            .filter(|(_, score)| *score >= self.confidence_threshold)
            .collect();
        // Stable sort: equal scores keep the tool order the caller supplied.
        qualifying.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let selected: Vec<String> = qualifying
            .into_iter()
            .take(self.max_tools)
            .map(|(name, _)| name)
            .collect();

        let mut recommendation = BTreeMap::new();
        if !selected.is_empty() {
            recommendation.insert("tools".to_string(), selected.join(","));
        }
        Ok(Decision::new(
            Status::Decided,
            Some(selected.clone()),
            if selected.is_empty() {
                "NONE_APPLICABLE"
            } else {
                "SELECTED"
            },
            recommendation,
        ))
    }
}

impl Middleware for JevToolSelectionMiddleware {
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
        if self.execution.mode() == Mode::Off {
            return next(input);
        }
        let ReasoningInput {
            messages,
            tools,
            options,
        } = input;
        let optional: Vec<ToolSchema> = tools
            .iter()
            .filter(|tool| !self.always_include.contains(&tool.name))
            .cloned()
            .collect();
        if optional.is_empty() || latest_user_text(&messages).is_empty() {
            return next(ReasoningInput::new(messages, tools, options));
        }
        let state = self.state_for(&messages);
        let this = self.clone();
        deferred(async move {
            let selector = this.clone();
            let decision = execute_guarded(&this.execution, async move {
                selector.select(&state, &optional).await
            })
            .await;
            if this.execution.mode() != Mode::Enforce || decision.status != Status::Decided {
                return next(ReasoningInput::new(messages, tools, options));
            }
            let selected = decision.value.clone().unwrap_or_default();
            let filtered = tools
                .iter()
                .filter(|tool| {
                    this.always_include.contains(&tool.name) || selected.contains(&tool.name)
                })
                .cloned()
                .collect();
            next(ReasoningInput::new(messages, filtered, options))
        })
    }
}

/// Incremental builder for [`JevToolSelectionMiddleware`] (`JevToolSelectionMiddleware.Builder`).
pub struct JevToolSelectionMiddlewareBuilder {
    call: Caller,
    always_include: Vec<String>,
    max_tools: usize,
    confidence_threshold: f64,
    rejection_threshold: f64,
    strategy: ContextStrategy,
    max_context_messages: usize,
    max_context_chars: usize,
    options: Options,
}

impl JevToolSelectionMiddlewareBuilder {
    /// Creates a builder with Java's defaults (disabled, `maxTools` 3, thresholds `0.8`/`0.2`).
    pub fn new(call: Caller) -> Self {
        Self {
            call,
            always_include: DEFAULT_ALWAYS_INCLUDE_TOOLS
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            max_tools: DEFAULT_MAX_TOOLS,
            confidence_threshold: DEFAULT_SELECTION_CONFIDENCE,
            rejection_threshold: DEFAULT_REJECTION_THRESHOLD,
            strategy: ContextStrategy::RecentWindow,
            max_context_messages: DEFAULT_MAX_CONTEXT_MESSAGES,
            max_context_chars: DEFAULT_MAX_CONTEXT_CHARS,
            options: Options::disabled(),
        }
    }

    /// Replaces the always-included tool names (`Builder.alwaysIncludeTools`).
    pub fn always_include_tools<S: AsRef<str>>(
        mut self,
        names: impl IntoIterator<Item = S>,
    ) -> Self {
        self.always_include = names
            .into_iter()
            .map(|name| name.as_ref().to_string())
            .collect();
        self
    }

    /// Sets the selection cap (`Builder.maxTools`).
    pub fn max_tools(mut self, max_tools: usize) -> Self {
        self.max_tools = max_tools;
        self
    }

    /// Sets the selection threshold (`Builder.confidenceThreshold`).
    pub fn confidence_threshold(mut self, threshold: f64) -> Self {
        self.confidence_threshold = threshold;
        self
    }

    /// Sets the ambiguity floor (`Builder.rejectionThreshold`).
    pub fn rejection_threshold(mut self, threshold: f64) -> Self {
        self.rejection_threshold = threshold;
        self
    }

    /// Sets the context strategy (`Builder.contextStrategy`).
    pub fn context_strategy(mut self, strategy: ContextStrategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Sets the recent-window message limit (`Builder.maxContextMessages`).
    pub fn max_context_messages(mut self, max_messages: usize) -> Self {
        self.max_context_messages = max_messages;
        self
    }

    /// Sets the recent-window character budget (`Builder.maxContextChars`).
    pub fn max_context_chars(mut self, max_chars: usize) -> Self {
        self.max_context_chars = max_chars;
        self
    }

    /// Sets the execution mode/budget (`Builder.execution`).
    pub fn execution(mut self, options: Options) -> Self {
        self.options = options;
        self
    }

    /// Validates and builds (`JevToolSelectionMiddleware` constructor).
    pub fn build(self) -> Result<JevToolSelectionMiddleware, JevError> {
        if self.max_context_messages == 0 || self.max_context_chars == 0 {
            return Err(JevError::invalid(
                "context message and character limits must be positive",
            ));
        }
        if self.max_tools == 0
            || !self.confidence_threshold.is_finite()
            || !self.rejection_threshold.is_finite()
            || self.rejection_threshold < 0.0
            || self.confidence_threshold > 1.0
            || self.rejection_threshold >= self.confidence_threshold
        {
            return Err(JevError::invalid(
                "require maxTools > 0 and 0 <= rejection < selection <= 1",
            ));
        }
        Ok(JevToolSelectionMiddleware {
            call: self.call,
            always_include: self.always_include,
            max_tools: self.max_tools,
            confidence_threshold: self.confidence_threshold,
            rejection_threshold: self.rejection_threshold,
            strategy: self.strategy,
            max_context_messages: self.max_context_messages,
            max_context_chars: self.max_context_chars,
            execution: JevExecution::new("tool-selection", self.options),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// 3. Automatic execution (`JevAutoModeMiddleware`)
// ---------------------------------------------------------------------------------------------

/// The calls an assessment allowed and denied (`RiskDecision`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RiskDecision {
    /// Calls that may run.
    pub allowed: Vec<String>,
    /// Calls that are denied.
    pub denied: Vec<String>,
}

/// Decides, before execution, whether a guarded tool call is safe to run automatically
/// (`JevAutoModeMiddleware`).
///
/// Calls the user explicitly allowed skip the check entirely, so this guardrail can never
/// override a human confirmation; a call whose approved arguments changed is denied without
/// asking JEV at all.
#[derive(Clone)]
pub struct JevAutoModeMiddleware {
    call: Caller,
    guarded_tools: Vec<String>,
    safety_threshold: f64,
    deny_message: String,
    execution: JevExecution,
}

impl std::fmt::Debug for JevAutoModeMiddleware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JevAutoModeMiddleware")
            .field("guarded_tools", &self.guarded_tools.len())
            .finish_non_exhaustive()
    }
}

impl JevAutoModeMiddleware {
    /// Starts a builder over an injected caller.
    pub fn builder(call: Caller) -> JevAutoModeMiddlewareBuilder {
        JevAutoModeMiddlewareBuilder::new(call)
    }

    /// Starts a builder over the real JEV client.
    pub fn from_client(client: JevClient) -> JevAutoModeMiddlewareBuilder {
        JevAutoModeMiddlewareBuilder::new(caller_from_client(client))
    }

    /// The configured execution mode.
    pub fn mode(&self) -> Mode {
        self.execution.mode()
    }

    /// The safety threshold.
    pub fn safety_threshold(&self) -> f64 {
        self.safety_threshold
    }

    /// The message written as the `DENIED` tool result.
    pub fn deny_message(&self) -> &str {
        &self.deny_message
    }

    /// Splits the acting input into the calls that need a check and the rest (`partition`).
    fn partition_calls(
        &self,
        ctx: &RuntimeContext,
        tool_calls: &[ToolUseBlock],
    ) -> (Vec<ToolUseBlock>, Vec<ToolUseBlock>) {
        let mut guarded = Vec::new();
        let mut rest = Vec::new();
        for call in tool_calls {
            let approved_by_user = call.state == ToolCallState::Allowed;
            let needs_check = self.guarded_tools.contains(&call.name)
                && (!approved_by_user || confirmed_calls::contains(ctx, call))
                && !confirmed_calls::matches(ctx, call);
            if needs_check {
                guarded.push(call.clone());
            } else {
                rest.push(call.clone());
            }
        }
        (guarded, rest)
    }

    /// Builds the risk request (`riskRequest`).
    fn risk_request(&self, messages: &[Msg], guarded: &[ToolUseBlock]) -> SystemOneRequest {
        let mut details = serde_json::Map::new();
        let mut builder = SystemOneRequest::builder();
        for (index, call) in guarded.iter().enumerate() {
            let question_id = format!("{QUESTION_ID_PREFIX}{index}");
            details.insert(
                question_id.clone(),
                json!({ "name": call.name, "input": Value::Object(call.input.clone()) }),
            );
            builder = builder.question(
                question_id.clone(),
                Question::Noul(NoulQuestion::new(
                    format!(
                        "Is the tool call described by state.tool_calls.{question_id} safe to execute, given the conversation in state.messages? Consider destructive operations, data loss, irreversible changes, and security implications."
                    ),
                    Some(crate::jev::dto::NoulCriteria::new(
                        "Yes, the operation is safe to execute automatically.",
                        "No, the operation is too risky to execute automatically.",
                    )),
                )),
            );
        }
        builder
            .state(json!({ "messages": messages, "tool_calls": details }))
            .build()
    }

    /// Folds the reply into allowed/denied call ids (`parseDecision`).
    fn parse(
        &self,
        result: &SystemOneResult,
        guarded: &[ToolUseBlock],
    ) -> Result<RiskDecision, JevError> {
        let answers = result.answers.as_ref().ok_or_else(|| JevError::invalid("invalid answers"))?;
        if answers.len() != guarded.len() {
            return Err(JevError::invalid("invalid answers"));
        }
        let mut decision = RiskDecision::default();
        for (index, call) in guarded.iter().enumerate() {
            let Some(Answer::Noul(NoulAnswer { noul: Some(score) })) =
                answers.get(&format!("{QUESTION_ID_PREFIX}{index}"))
            else {
                return Err(JevError::invalid("invalid risk answer"));
            };
            if !score.is_finite() || !(0.0..=1.0).contains(score) {
                return Err(JevError::invalid("invalid risk answer"));
            }
            if *score >= self.safety_threshold {
                decision.allowed.push(call.id.clone());
            } else {
                decision.denied.push(call.id.clone());
            }
        }
        Ok(decision)
    }

    async fn assess(
        &self,
        messages: Option<&[Msg]>,
        guarded: &[ToolUseBlock],
        confirmation_changed: bool,
    ) -> Result<Decision<RiskDecision>, JevError> {
        if confirmation_changed {
            return Ok(Decision::uncertain("CONFIRMATION_CHANGED"));
        }
        let Some(messages) = messages else {
            return Ok(Decision::uncertain("MISSING_STATE"));
        };
        let request = self.risk_request(messages, guarded);
        let result = (self.call)(request)
            .await
            .map_err(|failure| JevError::invalid(failure.message()))?;
        let decision = self.parse(&result, guarded)?;
        let mut recommendation = BTreeMap::new();
        recommendation.insert("deniedCalls".to_string(), decision.denied.join(","));
        Ok(Decision::new(
            Status::Decided,
            Some(decision.clone()),
            if decision.denied.is_empty() {
                "ALLOW"
            } else {
                "DENY"
            },
            recommendation,
        ))
    }
}

impl Middleware for JevAutoModeMiddleware {
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
        if self.execution.mode() == Mode::Off {
            return next(input);
        }
        let ActingInput { tool_calls, results } = input;
        let (guarded, _rest) = self.partition_calls(ctx, &tool_calls);
        if guarded.is_empty() {
            return next(ActingInput::with_results(tool_calls, results));
        }
        let messages = state_messages(ctx);
        let confirmation_changed = guarded
            .iter()
            .any(|call| confirmed_calls::contains(ctx, call) && !confirmed_calls::matches(ctx, call));
        let reply_id = reply_id(ctx);
        let this = self.clone();
        deferred(async move {
            let guard = this.clone();
            let assessed = guarded.clone();
            let decision = execute_guarded(&this.execution, async move {
                guard
                    .assess(messages.as_deref(), &assessed, confirmation_changed)
                    .await
            })
            .await;
            if this.execution.mode() != Mode::Enforce {
                return next(ActingInput::with_results(tool_calls, results));
            }
            // An error or an inconclusive assessment denies every guarded call, exactly like
            // Java's `d.status() == DECIDED ? d.value() : new RiskDecision(List.of(), guarded)`.
            let risk = match (decision.status, decision.value) {
                (Status::Decided, Some(risk)) => risk,
                _ => RiskDecision {
                    allowed: Vec::new(),
                    denied: guarded.iter().map(|call| call.id.clone()).collect(),
                },
            };
            if risk.denied.is_empty() {
                return next(ActingInput::with_results(tool_calls, results));
            }
            let denied: Vec<ToolUseBlock> = guarded
                .into_iter()
                .filter(|call| risk.denied.contains(&call.id))
                .collect();
            for call in &denied {
                results.record(denied_result(call, &this.deny_message), false);
            }
            let passing: Vec<ToolUseBlock> = tool_calls
                .iter()
                .filter(|call| !risk.denied.contains(&call.id))
                .cloned()
                .collect();
            let events = denied_events(&denied, &reply_id, &this.deny_message);
            let stream: EventStream = if passing.is_empty() {
                events
            } else {
                Box::pin(events.chain(next(ActingInput::with_results(passing, results))))
            };
            stream
        })
    }
}

/// Incremental builder for [`JevAutoModeMiddleware`] (`JevAutoModeMiddleware.Builder`).
pub struct JevAutoModeMiddlewareBuilder {
    call: Caller,
    guarded_tools: Vec<String>,
    safety_threshold: f64,
    deny_message: String,
    options: Options,
}

impl JevAutoModeMiddlewareBuilder {
    /// Creates a builder with Java's defaults (disabled, threshold `0.8`).
    pub fn new(call: Caller) -> Self {
        Self {
            call,
            guarded_tools: Vec::new(),
            safety_threshold: DEFAULT_SAFETY_THRESHOLD,
            deny_message: DEFAULT_DENY_MESSAGE.to_string(),
            options: Options::disabled(),
        }
    }

    /// Adds one guarded tool (`Builder.guardedTool`).
    pub fn guarded_tool(mut self, tool_name: impl Into<String>) -> Self {
        self.guarded_tools.push(tool_name.into());
        self
    }

    /// Replaces the guarded tool names (`Builder.guardedTools`).
    pub fn guarded_tools<S: AsRef<str>>(
        mut self,
        tool_names: impl IntoIterator<Item = S>,
    ) -> Self {
        self.guarded_tools = tool_names
            .into_iter()
            .map(|name| name.as_ref().to_string())
            .collect();
        self
    }

    /// Sets the safety threshold (`Builder.safetyThreshold`).
    pub fn safety_threshold(mut self, threshold: f64) -> Self {
        self.safety_threshold = threshold;
        self
    }

    /// Sets the denial message (`Builder.denyMessage`).
    pub fn deny_message(mut self, message: impl Into<String>) -> Self {
        self.deny_message = message.into();
        self
    }

    /// Sets the execution mode/budget (`Builder.execution`).
    pub fn execution(mut self, options: Options) -> Self {
        self.options = options;
        self
    }

    /// Validates and builds (`JevAutoModeMiddleware.validate`).
    pub fn build(self) -> Result<JevAutoModeMiddleware, JevError> {
        if self.guarded_tools.is_empty() {
            return Err(JevError::invalid("guardedTools must not be empty"));
        }
        if self.guarded_tools.iter().any(|name| name.trim().is_empty()) {
            return Err(JevError::invalid("guarded tool names must not be blank"));
        }
        if !self.safety_threshold.is_finite() || !(0.0..=1.0).contains(&self.safety_threshold) {
            return Err(JevError::invalid(
                "safetyThreshold must be between 0 and 1",
            ));
        }
        if self.deny_message.trim().is_empty() {
            return Err(JevError::invalid("denyMessage must not be blank"));
        }
        Ok(JevAutoModeMiddleware {
            call: self.call,
            guarded_tools: self.guarded_tools,
            safety_threshold: self.safety_threshold,
            deny_message: self.deny_message,
            execution: JevExecution::new("tool-guard", self.options),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Shared middleware plumbing
// ---------------------------------------------------------------------------------------------

/// Runs `work` once and flattens the stream it returns (`Flux.defer(...)`).
fn deferred<F>(work: F) -> EventStream
where
    F: Future<Output = EventStream> + Send + 'static,
{
    Box::pin(futures::stream::once(work).flatten())
}

/// The `DENIED` result a blocked call records.
fn denied_result(call: &ToolUseBlock, message: &str) -> ToolResultBlock {
    ToolResultBlock {
        id: Some(call.id.clone()),
        name: Some(call.name.clone()),
        output: vec![ContentBlock::Text(TextBlock::new(message))],
        metadata: Default::default(),
        state: ToolResultState::Denied,
    }
}

/// The synthetic `TOOL_RESULT_START` / `TEXT_DELTA` / `END(DENIED)` lifecycle per blocked call.
fn denied_events(denied: &[ToolUseBlock], reply_id: &str, message: &str) -> EventStream {
    let mut events: Vec<CoreResult<AgentEvent>> = Vec::new();
    for call in denied {
        let mut base = serde_json::Map::new();
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

/// The persisted conversation, when the runtime context carries an agent state.
fn state_messages(ctx: &RuntimeContext) -> Option<Vec<Msg>> {
    ctx.state.as_ref().map(|state| {
        state
            .lock()
            .map(|state| state.context().to_vec())
            .unwrap_or_default()
    })
}

/// The reply id of the current run (Java `state.getReplyId()`).
fn reply_id(ctx: &RuntimeContext) -> String {
    ctx.state
        .as_ref()
        .and_then(|state| state.lock().ok().map(|state| state.reply_id().to_string()))
        .unwrap_or_else(|| GUARD_REPLY_ID.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_core::middleware::ActingResultsHandle;
    use agentscope_core::model::{ChatResponse, GenerateOptions};
    use agentscope_core::state::AgentState;
    use futures::stream::BoxStream;
    use std::time::Duration;

    use crate::jev::call::testing::ScriptedCaller;
    use crate::jev::dto::Usage;
    use crate::jev::confirmed_calls::JevConfirmedCalls;

    struct StubModel {
        name: &'static str,
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
            true
        }
    }

    fn model(name: &'static str) -> Arc<dyn Model> {
        Arc::new(StubModel { name })
    }

    fn enforce() -> Options {
        Options::new(Mode::Enforce, Duration::from_secs(2), "v1").unwrap()
    }

    fn shadow() -> Options {
        Options::new(Mode::Shadow, Duration::from_secs(2), "v1").unwrap()
    }

    fn noul_reply(values: &[f64]) -> SystemOneResult {
        let mut answers = BTreeMap::new();
        for (index, value) in values.iter().enumerate() {
            answers.insert(
                format!("{QUESTION_ID_PREFIX}{index}"),
                Answer::Noul(NoulAnswer { noul: Some(*value) }),
            );
        }
        SystemOneResult {
            model: Some("jev-test".to_string()),
            answers: Some(answers),
            usage: Some(Usage::new(1, 1)),
        }
    }

    fn choice_reply(choice: &str, confidence: f64, options: &[(&str, f64)]) -> SystemOneResult {
        let mut distribution = BTreeMap::new();
        for (name, probability) in options {
            distribution.insert((*name).to_string(), *probability);
        }
        let mut answers = BTreeMap::new();
        answers.insert(
            ROUTING_QUESTION.to_string(),
            Answer::Choice(ChoiceAnswer {
                choice: Some(choice.to_string()),
                probabilities: Some(distribution),
                confidence: Some(confidence),
            }),
        );
        SystemOneResult {
            model: Some("jev-test".to_string()),
            answers: Some(answers),
            usage: Some(Usage::new(1, 1)),
        }
    }

    fn tool(name: &'static str) -> ToolSchema {
        ToolSchema::new(name, format!("{name} description"))
    }

    fn call(id: &str, name: &str, arguments: Value) -> ToolUseBlock {
        let Value::Object(input) = arguments else {
            panic!("arguments must be an object");
        };
        ToolUseBlock::new(id, name, input)
    }

    async fn drain(stream: EventStream) -> Vec<CoreResult<AgentEvent>> {
        stream.collect::<Vec<_>>().await
    }

    /// A context carrying a bound agent state (the auto-mode guard needs one).
    fn ctx_with_state(messages: Vec<Msg>) -> RuntimeContext {
        let mut ctx = RuntimeContext::new();
        let mut state = AgentState::default();
        state.context = messages;
        ctx.state = Some(Arc::new(Mutex::new(state)));
        ctx
    }

    // ---------------------------------------------------------------------------------------
    // Model routing
    // ---------------------------------------------------------------------------------------

    #[tokio::test]
    async fn the_router_is_a_passthrough_when_disabled() {
        let caller = Arc::new(ScriptedCaller::new(vec![Ok(choice_reply(
            "slow",
            0.99,
            &[("fast", 0.01), ("slow", 0.99)],
        ))]));
        let middleware = JevModelRouterMiddleware::builder(caller.as_caller())
            .choice("fast", model("fast"), "cheap")
            .choice("slow", model("slow"), "thorough")
            .execution(Options::disabled())
            .build()
            .unwrap();
        let ctx = RuntimeContext::new().with_session_id("s-off");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let stream = middleware.on_agent(
            &ctx,
            AgentInput::new(vec![Msg::user("hello")]),
            Box::new(move |input: AgentInput| -> EventStream {
                sink.lock().unwrap().push(input.messages.len());
                Box::pin(futures::stream::empty())
            }),
        );
        assert!(drain(stream).await.is_empty());
        assert!(caller.requests().is_empty(), "OFF never calls JEV");
        assert_eq!(*seen.lock().unwrap(), vec![1]);
        assert!(middleware.decision(&ctx).unwrap().is_fallback());
    }

    #[tokio::test]
    async fn enforce_routes_once_per_invocation_and_swaps_the_model() {
        let caller = Arc::new(ScriptedCaller::new(vec![Ok(choice_reply(
            "slow",
            0.9,
            &[("fast", 0.1), ("slow", 0.9)],
        ))]));
        let middleware = JevModelRouterMiddleware::builder(caller.as_caller())
            .choice("fast", model("fast"), "cheap")
            .choice("slow", model("slow"), "thorough")
            .execution(enforce())
            .build()
            .unwrap();
        let ctx = RuntimeContext::new().with_session_id("s-route");

        let stream = middleware.on_agent(
            &ctx,
            AgentInput::new(vec![Msg::user("solve this")]),
            Box::new(|_: AgentInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        assert!(drain(stream).await.is_empty());
        assert_eq!(caller.requests().len(), 1);

        let decision = middleware.decision(&ctx).unwrap();
        assert_eq!(decision.model_name(), Some("slow"));
        assert_eq!(decision.confidence, Some(0.9));
        assert_eq!(decision.probabilities.get("fast"), Some(&0.1));

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
    async fn shadow_mode_asks_but_never_switches_the_model() {
        let caller = Arc::new(ScriptedCaller::new(vec![Ok(choice_reply(
            "slow",
            0.99,
            &[("fast", 0.01), ("slow", 0.99)],
        ))]));
        let middleware = JevModelRouterMiddleware::builder(caller.as_caller())
            .choice("fast", model("fast"), "cheap")
            .choice("slow", model("slow"), "thorough")
            .execution(shadow())
            .build()
            .unwrap();
        let ctx = RuntimeContext::new().with_session_id("s-shadow");
        let stream = middleware.on_agent(
            &ctx,
            AgentInput::new(vec![Msg::user("solve this")]),
            Box::new(|_: AgentInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        assert!(drain(stream).await.is_empty());
        assert_eq!(caller.requests().len(), 1, "SHADOW still asks");
        assert!(
            middleware.decision(&ctx).unwrap().is_fallback(),
            "SHADOW must not store a route"
        );
    }

    #[tokio::test]
    async fn low_confidence_or_an_unknown_choice_keeps_the_fallback() {
        let replies = vec![
            choice_reply("slow", 0.5, &[("fast", 0.5), ("slow", 0.5)]),
            choice_reply("ghost", 0.99, &[("ghost", 0.99)]),
        ];
        for reply in replies {
            let caller = Arc::new(ScriptedCaller::new(vec![Ok(reply)]));
            let middleware = JevModelRouterMiddleware::builder(caller.as_caller())
                .choice("fast", model("fast"), "cheap")
                .choice("slow", model("slow"), "thorough")
                .execution(enforce())
                .build()
                .unwrap();
            let ctx = RuntimeContext::new().with_session_id("s-low");
            let stream = middleware.on_agent(
                &ctx,
                AgentInput::new(vec![Msg::user("hello")]),
                Box::new(|_: AgentInput| -> EventStream { Box::pin(futures::stream::empty()) }),
            );
            assert!(drain(stream).await.is_empty());
            assert!(middleware.decision(&ctx).unwrap().is_fallback());
        }
    }

    #[tokio::test]
    async fn a_request_with_tools_is_incompatible_and_keeps_the_caller_model() {
        let caller = Arc::new(ScriptedCaller::new(vec![Ok(choice_reply(
            "slow",
            0.99,
            &[("fast", 0.01), ("slow", 0.99)],
        ))]));
        let middleware = JevModelRouterMiddleware::builder(caller.as_caller())
            .choice("fast", model("fast"), "cheap")
            .choice("slow", model("slow"), "thorough")
            .execution(enforce())
            .build()
            .unwrap();
        let ctx = RuntimeContext::new().with_session_id("s-tools");
        let stream = middleware.on_agent(
            &ctx,
            AgentInput::new(vec![Msg::user("hello")]),
            Box::new(|_: AgentInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        assert!(drain(stream).await.is_empty());
        assert_eq!(middleware.decision(&ctx).unwrap().model_name(), Some("slow"));

        let stream = middleware.on_model_call(
            &ctx,
            ModelCallInput::new(
                vec![Msg::user("hi")],
                vec![tool("web_search")],
                GenerateOptions::default(),
                model("fast"),
            ),
            Box::new(|_: ModelCallInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        assert!(drain(stream).await.is_empty());
        assert!(
            middleware.decision(&ctx).unwrap().is_fallback(),
            "an incompatible request resets the route"
        );
    }

    #[tokio::test]
    async fn the_eligible_filter_removes_every_candidate() {
        let caller = Arc::new(ScriptedCaller::new(vec![Ok(choice_reply(
            "slow",
            0.99,
            &[("slow", 0.99)],
        ))]));
        let middleware = JevModelRouterMiddleware::builder(caller.as_caller())
            .choice("fast", model("fast"), "cheap")
            .choice("slow", model("slow"), "thorough")
            .eligible(Arc::new(|_, _| false))
            .execution(enforce())
            .build()
            .unwrap();
        let ctx = RuntimeContext::new().with_session_id("s-eligible");
        let stream = middleware.on_agent(
            &ctx,
            AgentInput::new(vec![Msg::user("hello")]),
            Box::new(|_: AgentInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        assert!(drain(stream).await.is_empty());
        assert!(caller.requests().is_empty());
        assert!(middleware.decision(&ctx).unwrap().is_fallback());
    }

    #[test]
    fn the_router_validates_its_configuration() {
        let caller = Arc::new(ScriptedCaller::new(Vec::new())).as_caller();
        let error = JevModelRouterMiddleware::builder(caller.clone())
            .build()
            .unwrap_err();
        assert_eq!(error.message(), "at least one model choice are required");

        let error = JevModelRouterMiddleware::builder(caller.clone())
            .choice("  ", model("fast"), "cheap")
            .build()
            .unwrap_err();
        assert_eq!(error.message(), "choice names must not be blank");

        let error = JevModelRouterMiddleware::builder(caller.clone())
            .choice("fast", model("fast"), " ")
            .build()
            .unwrap_err();
        assert_eq!(error.message(), "criteria must not be blank");

        let error = JevModelRouterMiddleware::builder(caller)
            .choice("fast", model("fast"), "cheap")
            .confidence_threshold(1.5)
            .build()
            .unwrap_err();
        assert_eq!(
            error.message(),
            "confidenceThreshold must be between 0 and 1"
        );
    }

    // ---------------------------------------------------------------------------------------
    // Tool selection
    // ---------------------------------------------------------------------------------------

    fn reasoning_input(names: &[&'static str]) -> ReasoningInput {
        ReasoningInput::new(
            vec![Msg::user("find the docs")],
            names.iter().map(|name| tool(*name)).collect(),
            GenerateOptions::default(),
        )
    }

    #[tokio::test]
    async fn tool_selection_is_a_passthrough_when_disabled() {
        let caller = Arc::new(ScriptedCaller::new(vec![Ok(noul_reply(&[0.99]))]));
        let middleware = JevToolSelectionMiddleware::builder(caller.as_caller())
            .always_include_tools(Vec::<String>::new())
            .execution(Options::disabled())
            .build()
            .unwrap();
        let seen = Arc::new(Mutex::new(0usize));
        let sink = Arc::clone(&seen);
        let stream = middleware.on_reasoning(
            &RuntimeContext::new(),
            reasoning_input(&["web_search"]),
            Box::new(move |input: ReasoningInput| -> EventStream {
                *sink.lock().unwrap() += input.tools.len();
                Box::pin(futures::stream::empty())
            }),
        );
        assert!(drain(stream).await.is_empty());
        assert!(caller.requests().is_empty());
        assert_eq!(*seen.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn enforce_keeps_only_the_tools_above_the_threshold_in_score_order() {
        let caller = Arc::new(ScriptedCaller::new(vec![Ok(noul_reply(&[0.1, 0.95, 0.9]))]));
        let middleware = JevToolSelectionMiddleware::builder(caller.as_caller())
            .always_include_tools(Vec::<String>::new())
            .max_tools(1)
            .execution(enforce())
            .build()
            .unwrap();
        let kept = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&kept);
        let stream = middleware.on_reasoning(
            &RuntimeContext::new(),
            reasoning_input(&["low", "best", "second"]),
            Box::new(move |input: ReasoningInput| -> EventStream {
                *sink.lock().unwrap() = input
                    .tools
                    .iter()
                    .map(|tool| tool.name.clone())
                    .collect();
                Box::pin(futures::stream::empty())
            }),
        );
        assert!(drain(stream).await.is_empty());
        assert_eq!(*kept.lock().unwrap(), vec!["best".to_string()]);
    }

    #[tokio::test]
    async fn an_ambiguous_score_leaves_the_tool_set_untouched() {
        let caller = Arc::new(ScriptedCaller::new(vec![Ok(noul_reply(&[0.95, 0.5]))]));
        let middleware = JevToolSelectionMiddleware::builder(caller.as_caller())
            .always_include_tools(Vec::<String>::new())
            .execution(enforce())
            .build()
            .unwrap();
        let kept = Arc::new(Mutex::new(0usize));
        let sink = Arc::clone(&kept);
        let stream = middleware.on_reasoning(
            &RuntimeContext::new(),
            reasoning_input(&["best", "maybe"]),
            Box::new(move |input: ReasoningInput| -> EventStream {
                *sink.lock().unwrap() = input.tools.len();
                Box::pin(futures::stream::empty())
            }),
        );
        assert!(drain(stream).await.is_empty());
        assert_eq!(*kept.lock().unwrap(), 2, "ambiguous input is not filtered");
    }

    #[tokio::test]
    async fn always_included_tools_are_never_judged() {
        let caller = Arc::new(ScriptedCaller::new(vec![Ok(noul_reply(&[0.99]))]));
        let middleware = JevToolSelectionMiddleware::builder(caller.as_caller())
            .always_include_tools(["load_skill_through_path"])
            .execution(enforce())
            .build()
            .unwrap();
        let stream = middleware.on_reasoning(
            &RuntimeContext::new(),
            reasoning_input(&["load_skill_through_path"]),
            Box::new(|_: ReasoningInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        assert!(drain(stream).await.is_empty());
        assert!(caller.requests().is_empty(), "nothing optional to judge");
    }

    #[tokio::test]
    async fn a_malformed_relevance_answer_keeps_the_full_tool_set() {
        let caller = Arc::new(ScriptedCaller::new(vec![Ok(noul_reply(&[0.1]))]));
        let middleware = JevToolSelectionMiddleware::builder(caller.as_caller())
            .always_include_tools(Vec::<String>::new())
            .execution(enforce())
            .build()
            .unwrap();
        let kept = Arc::new(Mutex::new(0usize));
        let sink = Arc::clone(&kept);
        let stream = middleware.on_reasoning(
            &RuntimeContext::new(),
            reasoning_input(&["a", "b"]),
            Box::new(move |input: ReasoningInput| -> EventStream {
                *sink.lock().unwrap() = input.tools.len();
                Box::pin(futures::stream::empty())
            }),
        );
        assert!(drain(stream).await.is_empty());
        assert_eq!(*kept.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn shadow_mode_asks_but_does_not_filter() {
        let caller = Arc::new(ScriptedCaller::new(vec![Ok(noul_reply(&[0.01, 0.99]))]));
        let middleware = JevToolSelectionMiddleware::builder(caller.as_caller())
            .always_include_tools(Vec::<String>::new())
            .execution(shadow())
            .build()
            .unwrap();
        let kept = Arc::new(Mutex::new(0usize));
        let sink = Arc::clone(&kept);
        let stream = middleware.on_reasoning(
            &RuntimeContext::new(),
            reasoning_input(&["unused", "used"]),
            Box::new(move |input: ReasoningInput| -> EventStream {
                *sink.lock().unwrap() = input.tools.len();
                Box::pin(futures::stream::empty())
            }),
        );
        assert!(drain(stream).await.is_empty());
        assert_eq!(caller.requests().len(), 1);
        assert_eq!(*kept.lock().unwrap(), 2);
    }

    #[test]
    fn tool_selection_validates_its_thresholds() {
        let caller = Arc::new(ScriptedCaller::new(Vec::new())).as_caller();
        let error = JevToolSelectionMiddleware::builder(caller.clone())
            .max_tools(0)
            .build()
            .unwrap_err();
        assert_eq!(
            error.message(),
            "require maxTools > 0 and 0 <= rejection < selection <= 1"
        );

        let error = JevToolSelectionMiddleware::builder(caller.clone())
            .confidence_threshold(0.5)
            .rejection_threshold(0.6)
            .build()
            .unwrap_err();
        assert_eq!(
            error.message(),
            "require maxTools > 0 and 0 <= rejection < selection <= 1"
        );

        let error = JevToolSelectionMiddleware::builder(caller)
            .max_context_messages(0)
            .build()
            .unwrap_err();
        assert_eq!(
            error.message(),
            "context message and character limits must be positive"
        );
    }

    // ---------------------------------------------------------------------------------------
    // Automatic execution
    // ---------------------------------------------------------------------------------------

    fn guard(caller: Caller, options: Options) -> JevAutoModeMiddleware {
        JevAutoModeMiddleware::builder(caller)
            .guarded_tools(["refund"])
            .execution(options)
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn the_guard_is_a_passthrough_when_disabled() {
        let calls = Arc::new(ScriptedCaller::new(vec![Ok(noul_reply(&[0.01]))]));
        let middleware = guard(calls.as_caller(), Options::disabled());
        let results = ActingResultsHandle::new();
        let input = ActingInput::with_results(
            vec![call("call-1", "refund", json!({ "amount": 1 }))],
            results.clone(),
        );
        let stream = middleware.on_acting(
            &ctx_with_state(vec![Msg::user("refund it")]),
            input,
            Box::new(|_: ActingInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        assert!(drain(stream).await.is_empty());
        assert!(calls.requests().is_empty());
        assert!(results.snapshot().results.is_empty());
    }

    #[tokio::test]
    async fn enforce_denies_a_call_below_the_safety_threshold() {
        let calls = Arc::new(ScriptedCaller::new(vec![Ok(noul_reply(&[0.1]))]));
        let middleware = guard(calls.as_caller(), enforce());
        let results = ActingResultsHandle::new();
        let input = ActingInput::with_results(
            vec![call("call-1", "refund", json!({ "amount": 100 }))],
            results.clone(),
        );
        let forwarded = Arc::new(Mutex::new(false));
        let sink = Arc::clone(&forwarded);
        let stream = middleware.on_acting(
            &ctx_with_state(vec![Msg::user("refund it")]),
            input,
            Box::new(move |_: ActingInput| -> EventStream {
                *sink.lock().unwrap() = true;
                Box::pin(futures::stream::empty())
            }),
        );
        let events = drain(stream).await;
        assert_eq!(events.len(), 3, "start / delta / end(DENIED)");
        assert!(events.iter().all(|event| event.is_ok()));
        assert!(!*forwarded.lock().unwrap(), "the denied call never ran");

        let snapshot = results.snapshot();
        assert_eq!(snapshot.results.len(), 1);
        assert_eq!(snapshot.results[0].id.as_deref(), Some("call-1"));
        assert_eq!(snapshot.results[0].state, ToolResultState::Denied);
        assert!(snapshot.results[0]
            .output
            .iter()
            .any(|block| matches!(block, ContentBlock::Text(text) if text.text == DEFAULT_DENY_MESSAGE)));
    }

    #[tokio::test]
    async fn enforce_forwards_a_safe_call_unchanged() {
        let calls = Arc::new(ScriptedCaller::new(vec![Ok(noul_reply(&[0.95]))]));
        let middleware = guard(calls.as_caller(), enforce());
        let results = ActingResultsHandle::new();
        let input = ActingInput::with_results(
            vec![call("call-1", "refund", json!({ "amount": 1 }))],
            results.clone(),
        );
        let forwarded = Arc::new(Mutex::new(0usize));
        let sink = Arc::clone(&forwarded);
        let stream = middleware.on_acting(
            &ctx_with_state(vec![Msg::user("refund it")]),
            input,
            Box::new(move |input: ActingInput| -> EventStream {
                *sink.lock().unwrap() = input.tool_calls.len();
                Box::pin(futures::stream::empty())
            }),
        );
        assert!(drain(stream).await.is_empty());
        assert_eq!(*forwarded.lock().unwrap(), 1);
        assert!(results.snapshot().results.is_empty());
    }

    #[tokio::test]
    async fn unguarded_and_user_allowed_calls_skip_the_check() {
        let calls = Arc::new(ScriptedCaller::new(vec![Ok(noul_reply(&[0.95]))]));
        let middleware = guard(calls.as_caller(), enforce());
        let mut allowed = call("call-1", "refund", json!({ "amount": 1 }));
        allowed.state = ToolCallState::Allowed;
        let tool_calls = vec![allowed, call("call-2", "search", json!({}))];
        let stream = middleware.on_acting(
            &ctx_with_state(vec![Msg::user("refund it")]),
            ActingInput::new(tool_calls),
            Box::new(|_: ActingInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        assert!(drain(stream).await.is_empty());
        assert!(calls.requests().is_empty(), "nothing needed a judgement");
    }

    #[tokio::test]
    async fn a_malformed_risk_answer_denies_the_whole_batch() {
        let calls = Arc::new(ScriptedCaller::new(vec![Ok(noul_reply(&[0.95]))]));
        let middleware = guard(calls.as_caller(), enforce());
        let results = ActingResultsHandle::new();
        let tool_calls = vec![
            call("call-1", "refund", json!({ "amount": 1 })),
            call("call-2", "refund", json!({ "amount": 2 })),
        ];
        let stream = middleware.on_acting(
            &ctx_with_state(vec![Msg::user("refund it")]),
            ActingInput::with_results(tool_calls, results.clone()),
            Box::new(|_: ActingInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        assert_eq!(drain(stream).await.len(), 6, "three events per denial");
        assert_eq!(results.snapshot().results.len(), 2);
    }

    #[tokio::test]
    async fn a_changed_confirmation_is_denied_without_asking() {
        let calls = Arc::new(ScriptedCaller::new(vec![Ok(noul_reply(&[0.99]))]));
        let middleware = guard(calls.as_caller(), enforce());
        let mut ctx = ctx_with_state(vec![Msg::user("refund it")]);
        let ledger = JevConfirmedCalls::install(&mut ctx);
        ledger.remember_call(&call("call-1", "refund", json!({ "amount": 1 })));

        let results = ActingResultsHandle::new();
        let input = ActingInput::with_results(
            vec![call("call-1", "refund", json!({ "amount": 100 }))],
            results.clone(),
        );
        let stream = middleware.on_acting(
            &ctx,
            input,
            Box::new(|_: ActingInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        assert_eq!(drain(stream).await.len(), 3);
        assert!(calls.requests().is_empty(), "a changed approval is not re-judged");
        assert_eq!(
            results.snapshot().results[0].state,
            ToolResultState::Denied
        );
    }

    #[tokio::test]
    async fn a_missing_agent_state_denies_the_guarded_calls() {
        let calls = Arc::new(ScriptedCaller::new(vec![Ok(noul_reply(&[0.99]))]));
        let middleware = guard(calls.as_caller(), enforce());
        let results = ActingResultsHandle::new();
        let stream = middleware.on_acting(
            &RuntimeContext::new(),
            ActingInput::with_results(
                vec![call("call-1", "refund", json!({ "amount": 1 }))],
                results.clone(),
            ),
            Box::new(|_: ActingInput| -> EventStream { Box::pin(futures::stream::empty()) }),
        );
        assert_eq!(drain(stream).await.len(), 3);
        assert!(calls.requests().is_empty());
        assert_eq!(
            results.snapshot().results[0].state,
            ToolResultState::Denied
        );
    }

    #[test]
    fn the_guard_validates_its_configuration() {
        let caller = Arc::new(ScriptedCaller::new(Vec::new())).as_caller();
        let error = JevAutoModeMiddleware::builder(caller.clone())
            .build()
            .unwrap_err();
        assert_eq!(error.message(), "guardedTools must not be empty");

        let error = JevAutoModeMiddleware::builder(caller.clone())
            .guarded_tools(["  "])
            .build()
            .unwrap_err();
        assert_eq!(error.message(), "guarded tool names must not be blank");

        let error = JevAutoModeMiddleware::builder(caller.clone())
            .guarded_tools(["refund"])
            .safety_threshold(1.2)
            .build()
            .unwrap_err();
        assert_eq!(error.message(), "safetyThreshold must be between 0 and 1");

        let error = JevAutoModeMiddleware::builder(caller)
            .guarded_tools(["refund"])
            .deny_message(" ")
            .build()
            .unwrap_err();
        assert_eq!(error.message(), "denyMessage must not be blank");
    }

    // ---------------------------------------------------------------------------------------
    // JevSelectionSupport
    // ---------------------------------------------------------------------------------------

    #[test]
    fn the_support_projection_matches_java() {
        assert_eq!(latest_user_text(&[]), "");
        assert_eq!(
            latest_user_text(&[Msg::user(" first "), Msg::assistant("reply")]),
            "first"
        );
        assert_eq!(
            latest_user_state("hi"),
            json!({ "userRequest": "hi" })
        );

        let window = recent_window_state(&[Msg::user("old"), Msg::assistant("new")], 1, 100);
        assert_eq!(
            window,
            json!({ "messages": [{ "role": "assistant", "text": "new" }] })
        );
        assert_eq!(
            recent_window_state(&[], 8, 8000),
            json!({ "messages": [] })
        );

        let criteria = tool_criteria(&[tool("web_search")]);
        assert_eq!(criteria.len(), 2);
        assert!(criteria.contains_key("web_search"));
        assert!(criteria.contains_key(NONE_OPTION));

        assert_eq!(
            partition(&[1, 2, 3], 2),
            vec![vec![1, 2], vec![3]]
        );
        assert!(partition::<u8>(&[], 2).is_empty());
        assert!(partition(&[1], 0).is_empty());
    }

    #[test]
    fn selected_names_and_top_name_respect_none_and_confidence() {
        let mut distribution = BTreeMap::new();
        distribution.insert("a".to_string(), 0.6);
        distribution.insert("b".to_string(), 0.3);
        distribution.insert(NONE_OPTION.to_string(), 0.1);
        let answer = ChoiceAnswer {
            choice: Some("a".to_string()),
            probabilities: Some(distribution),
            confidence: Some(0.9),
        };

        assert_eq!(selected_names(&answer, 2, 0.8), vec!["a", "b"]);
        assert_eq!(selected_names(&answer, 1, 0.8), vec!["a"]);
        assert!(selected_names(&answer, 2, 0.95).is_empty());
        assert_eq!(top_name(&answer).as_deref(), Some("a"));

        let none_wins = ChoiceAnswer {
            choice: Some(NONE_OPTION.to_string()),
            probabilities: Some(BTreeMap::from([(NONE_OPTION.to_string(), 0.8)])),
            confidence: Some(0.99),
        };
        assert!(selected_names(&none_wins, 3, 0.8).is_empty());
        assert_eq!(top_name(&none_wins), None);
    }

}
