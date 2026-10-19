//! Middleware trait and chain for intercepting agent execution.
use std::sync::Arc;

use super::input::{ActingInput, AgentInput, EventStream, ModelCallInput, ReasoningInput};
use crate::agent::RuntimeContext;

/// A boxed continuation for the next middleware or core logic.
pub type Next<T> = Box<dyn FnOnce(T) -> EventStream + Send>;

/// Middleware provides interception at key execution points (onion pattern).
pub trait Middleware: Send + Sync {
    /// Intercepts the entire agent invocation.
    fn on_agent(
        &self,
        _ctx: &RuntimeContext,
        input: AgentInput,
        next: Next<AgentInput>,
    ) -> EventStream {
        next(input)
    }

    /// Intercepts the reasoning (model-call) phase.
    fn on_reasoning(
        &self,
        _ctx: &RuntimeContext,
        input: ReasoningInput,
        next: Next<ReasoningInput>,
    ) -> EventStream {
        next(input)
    }

    /// Intercepts the tool-call execution phase.
    fn on_acting(
        &self,
        _ctx: &RuntimeContext,
        input: ActingInput,
        next: Next<ActingInput>,
    ) -> EventStream {
        next(input)
    }

    /// Intercepts the raw model API call.
    fn on_model_call(
        &self,
        _ctx: &RuntimeContext,
        input: ModelCallInput,
        next: Next<ModelCallInput>,
    ) -> EventStream {
        next(input)
    }

    /// Transforms the system prompt (pipeline pattern).
    fn on_system_prompt(&self, _ctx: &RuntimeContext, current_prompt: String) -> String {
        current_prompt
    }

    /// Notified once per call as soon as the call-scoped `AgentState` is bound to
    /// the runtime context, before the input enters the pipeline.
    ///
    /// This is the window the other hooks miss: `on_agent` is too early (the state
    /// is not yet bound) and `on_reasoning`/`on_acting` are too late. It enables
    /// session bootstrapping, state-driven decisions and per-session auditing.
    ///
    /// A pure notification (no `next` delegation): it may mutate `state`, but a
    /// panic/exception propagates to the caller unchanged. Mirrors Java
    /// `MiddlewareBase.onAgentStateReady`; the default is a no-op.
    fn on_agent_state_ready(
        &self,
        _ctx: &RuntimeContext,
        _state: &mut crate::state::AgentState,
        _input_messages: &[crate::message::Msg],
    ) {
    }

    /// The extension points where this middleware is active.
    ///
    /// This is a **participation switch**, not a statement about which methods are
    /// overridden: a point declared active but not overridden simply runs its
    /// default (pass-through / no-op), and a point omitted is never invoked — even
    /// if overridden. The default is active at every point (including ones added in
    /// future releases), so middlewares that do not override this keep today's
    /// behaviour. An empty set disables the middleware at every point while keeping
    /// its registration.
    ///
    /// Activation is decided once when the agent is built. Mirrors Java
    /// `MiddlewareBase.activePoints()`.
    fn active_points(&self) -> Vec<ExtensionPoint> {
        ExtensionPoint::ALL.to_vec()
    }
}

/// The middleware extension points, mirroring Java `MiddlewareBase.ExtensionPoint`.
///
/// Used by [`Middleware::active_points`] to declare participation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExtensionPoint {
    OnAgent,
    OnReasoning,
    OnActing,
    OnModelCall,
    OnSystemPrompt,
    OnAgentStateReady,
}

impl ExtensionPoint {
    /// Every extension point, in declaration order.
    pub const ALL: [ExtensionPoint; 6] = [
        ExtensionPoint::OnAgent,
        ExtensionPoint::OnReasoning,
        ExtensionPoint::OnActing,
        ExtensionPoint::OnModelCall,
        ExtensionPoint::OnSystemPrompt,
        ExtensionPoint::OnAgentStateReady,
    ];
}

/// A chain of middleware executed in order (onion pattern).
///
/// Middlewares are stored behind `Arc` so the chain can be shared across calls and
/// the individual middleware references can be moved into `'static` closures.
#[derive(Clone)]
pub struct MiddlewareChain {
    middlewares: Vec<Arc<dyn Middleware>>,
}

impl Default for MiddlewareChain {
    fn default() -> Self {
        Self::new()
    }
}

impl MiddlewareChain {
    /// Creates an empty middleware chain.
    pub fn new() -> Self {
        Self { middlewares: Vec::new() }
    }

    /// Adds a middleware to the chain.
    pub fn add(&mut self, mw: Box<dyn Middleware>) {
        self.middlewares.push(Arc::from(mw));
    }

    /// Adds an already-`Arc`-wrapped middleware to the chain.
    ///
    /// Useful when a middleware must also be shared elsewhere (e.g. a lazy tool
    /// holding the same `Arc`) — avoids cloning the concrete type (嵌入式优化).
    pub fn add_arc(&mut self, mw: Arc<dyn Middleware>) {
        self.middlewares.push(mw);
    }

    /// Returns the number of middlewares in the chain.
    pub fn len(&self) -> usize {
        self.middlewares.len()
    }

    /// Whether the chain is empty.
    pub fn is_empty(&self) -> bool {
        self.middlewares.is_empty()
    }

    /// The middlewares participating in the given extension point, in registration
    /// order.
    ///
    /// A middleware participates when its [`Middleware::active_points`] contains
    /// `point`. Points with no participants yield an empty slice, which lets the
    /// caller skip building a wrapper layer entirely (Java `activePoints()`).
    pub fn participants(&self, point: ExtensionPoint) -> Vec<&Arc<dyn Middleware>> {
        self.middlewares
            .iter()
            .filter(|mw| mw.active_points().contains(&point))
            .collect()
    }

    /// Notifies every participating middleware that this call's `AgentState` is
    /// ready, in registration order.
    ///
    /// Mirrors Java `ReActAgent.beforeAgentExecution`: invoked once per call right
    /// after the call-scoped state is bound, before the input enters the pipeline.
    /// A middleware's panic propagates unchanged and the remaining participants are
    /// not invoked (Java: "a consumer exception propagates ... and the remaining
    /// consumers are not invoked").
    pub fn notify_agent_state_ready(
        &self,
        ctx: &RuntimeContext,
        state: &mut crate::state::AgentState,
        input_messages: &[crate::message::Msg],
    ) {
        for mw in self.participants(ExtensionPoint::OnAgentStateReady) {
            mw.on_agent_state_ready(ctx, state, input_messages);
        }
    }

    /// Applies the chain's system-prompt transformers in order, skipping
    /// middlewares that do not participate in `OnSystemPrompt`.
    pub fn apply_system_prompt(&self, ctx: &RuntimeContext, prompt: String) -> String {
        self.participants(ExtensionPoint::OnSystemPrompt)
            .iter()
            .fold(prompt, |p, mw| mw.on_system_prompt(ctx, p))
    }

    /// Builds the event stream for an agent invocation, wrapping the core logic.
    pub fn wrap_agent(
        &self,
        ctx: RuntimeContext,
        input: AgentInput,
        core: Box<dyn FnOnce(AgentInput) -> EventStream + Send>,
    ) -> EventStream {
        let participants: Vec<Arc<dyn Middleware>> = self
            .participants(ExtensionPoint::OnAgent)
            .into_iter()
            .map(Arc::clone)
            .collect();
        let mut pipeline: Next<AgentInput> = Box::new(core);
        for mw in participants.into_iter().rev() {
            let prev = pipeline;
            let ctx = ctx.clone();
            pipeline = Box::new(move |input| mw.on_agent(&ctx, input, prev));
        }
        pipeline(input)
    }

    /// Builds the event stream for the reasoning (model-call) phase.
    pub fn wrap_reasoning(
        &self,
        ctx: RuntimeContext,
        input: ReasoningInput,
        core: Box<dyn FnOnce(ReasoningInput) -> EventStream + Send>,
    ) -> EventStream {
        let participants: Vec<Arc<dyn Middleware>> = self
            .participants(ExtensionPoint::OnReasoning)
            .into_iter()
            .map(Arc::clone)
            .collect();
        let mut pipeline: Next<ReasoningInput> = Box::new(core);
        for mw in participants.into_iter().rev() {
            let prev = pipeline;
            let ctx = ctx.clone();
            pipeline = Box::new(move |input| mw.on_reasoning(&ctx, input, prev));
        }
        pipeline(input)
    }

    /// Builds the event stream for the acting (tool-call execution) phase.
    pub fn wrap_acting(
        &self,
        ctx: RuntimeContext,
        input: ActingInput,
        core: Box<dyn FnOnce(ActingInput) -> EventStream + Send>,
    ) -> EventStream {
        let participants: Vec<Arc<dyn Middleware>> = self
            .participants(ExtensionPoint::OnActing)
            .into_iter()
            .map(Arc::clone)
            .collect();
        let mut pipeline: Next<ActingInput> = Box::new(core);
        for mw in participants.into_iter().rev() {
            let prev = pipeline;
            let ctx = ctx.clone();
            pipeline = Box::new(move |input| mw.on_acting(&ctx, input, prev));
        }
        pipeline(input)
    }

    /// Builds the event stream for the raw model API call.
    pub fn wrap_model_call(
        &self,
        ctx: RuntimeContext,
        input: ModelCallInput,
        core: Box<dyn FnOnce(ModelCallInput) -> EventStream + Send>,
    ) -> EventStream {
        let participants: Vec<Arc<dyn Middleware>> = self
            .participants(ExtensionPoint::OnModelCall)
            .into_iter()
            .map(Arc::clone)
            .collect();
        let mut pipeline: Next<ModelCallInput> = Box::new(core);
        for mw in participants.into_iter().rev() {
            let prev = pipeline;
            let ctx = ctx.clone();
            pipeline = Box::new(move |input| mw.on_model_call(&ctx, input, prev));
        }
        pipeline(input)
    }
}
