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

    /// Returns the number of middlewares in the chain.
    pub fn len(&self) -> usize {
        self.middlewares.len()
    }

    /// Whether the chain is empty.
    pub fn is_empty(&self) -> bool {
        self.middlewares.is_empty()
    }

    /// Applies the chain's system-prompt transformers in order.
    pub fn apply_system_prompt(&self, ctx: &RuntimeContext, prompt: String) -> String {
        self.middlewares
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
        let mut pipeline: Next<AgentInput> = Box::new(core);
        for mw in self.middlewares.iter().rev() {
            let prev = pipeline;
            let ctx = ctx.clone();
            let mw = Arc::clone(mw);
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
        let mut pipeline: Next<ReasoningInput> = Box::new(core);
        for mw in self.middlewares.iter().rev() {
            let prev = pipeline;
            let ctx = ctx.clone();
            let mw = Arc::clone(mw);
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
        let mut pipeline: Next<ActingInput> = Box::new(core);
        for mw in self.middlewares.iter().rev() {
            let prev = pipeline;
            let ctx = ctx.clone();
            let mw = Arc::clone(mw);
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
        let mut pipeline: Next<ModelCallInput> = Box::new(core);
        for mw in self.middlewares.iter().rev() {
            let prev = pipeline;
            let ctx = ctx.clone();
            let mw = Arc::clone(mw);
            pipeline = Box::new(move |input| mw.on_model_call(&ctx, input, prev));
        }
        pipeline(input)
    }
}
