//! Input types for middleware interception points.
use std::sync::Arc;

use crate::message::{Msg, ToolUseBlock};
use crate::model::{GenerateOptions, Model, ToolSchema};

/// Input for the agent invocation interception point.
#[derive(Debug, Clone)]
pub struct AgentInput {
    /// The input messages.
    pub messages: Vec<Msg>,
}

impl AgentInput {
    pub fn new(messages: Vec<Msg>) -> Self {
        Self { messages }
    }
}

/// Input for the reasoning (model-call) interception point.
#[derive(Debug, Clone)]
pub struct ReasoningInput {
    /// The messages sent to the model.
    pub messages: Vec<Msg>,
    /// The tool schemas available to the model.
    pub tools: Vec<ToolSchema>,
    /// The generation options.
    pub options: GenerateOptions,
}

impl ReasoningInput {
    pub fn new(messages: Vec<Msg>, tools: Vec<ToolSchema>, options: GenerateOptions) -> Self {
        Self { messages, tools, options }
    }
}

/// Input for the tool-call execution interception point.
#[derive(Debug, Clone)]
pub struct ActingInput {
    /// The tool call blocks to execute.
    pub tool_calls: Vec<ToolUseBlock>,
}

impl ActingInput {
    pub fn new(tool_calls: Vec<ToolUseBlock>) -> Self {
        Self { tool_calls }
    }
}

/// Input for the raw model API call interception point.
#[derive(Clone)]
pub struct ModelCallInput {
    /// The messages sent to the model.
    pub messages: Vec<Msg>,
    /// The tool schemas available to the model.
    pub tools: Vec<ToolSchema>,
    /// The generation options.
    pub options: GenerateOptions,
    /// The model being invoked.
    pub model: Arc<dyn Model>,
}

impl ModelCallInput {
    pub fn new(
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
        model: Arc<dyn Model>,
    ) -> Self {
        Self { messages, tools, options, model }
    }
}

/// A boxed event stream produced by a middleware chain step.
pub type EventStream = futures::stream::BoxStream<'static, crate::error::Result<crate::event::AgentEvent>>;
