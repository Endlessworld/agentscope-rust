//! Tool abstraction for agent tools, mirroring the Java `AgentTool`/`ToolBase` hierarchy.
use async_trait::async_trait;

use crate::error::Result;
use crate::message::{JsonMap, ToolResultBlock, ToolUseBlock};
use crate::model::ToolSchema;

use super::emitter::ToolEmitter;

/// Execution context passed to a tool invocation.
#[derive(Clone, Default)]
pub struct ToolExecutionContext {
    /// The tool call that triggered this execution.
    pub tool_call: Option<ToolUseBlock>,
    /// Additional runtime context.
    pub runtime: JsonMap,
    /// Optional streaming emitter for intermediate progress chunks. When
    /// absent, [`emit_to`](super::emitter::emit_to) falls back to a no-op.
    pub emitter: Option<std::sync::Arc<dyn ToolEmitter>>,
}

impl std::fmt::Debug for ToolExecutionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolExecutionContext")
            .field("tool_call", &self.tool_call)
            .field("runtime", &self.runtime)
            .field("has_emitter", &self.emitter.is_some())
            .finish()
    }
}

/// A tool that can be invoked by an AI agent.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool name (snake_case).
    fn name(&self) -> &str;
    /// The tool description shown to the model.
    fn description(&self) -> &str;
    /// Whether this tool performs no observable side effects.
    fn is_read_only(&self) -> bool {
        false
    }
    /// Whether this tool is executed outside the framework.
    fn is_external(&self) -> bool {
        false
    }
    /// Whether multiple invocations may run in parallel.
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    /// The JSON Schema for the tool parameters.
    fn parameters_schema(&self) -> serde_json::Value;
    /// Executes the tool with the given input arguments.
    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> Result<ToolResultBlock>;
    /// Returns the full tool schema.
    fn schema(&self) -> ToolSchema {
        let mut parameters = match self.parameters_schema() {
            serde_json::Value::Object(map) => map,
            other => {
                let mut m = serde_json::Map::new();
                m.insert("type".into(), serde_json::Value::String("object".into()));
                m.insert("properties".into(), other);
                m
            }
        };
        if !parameters.contains_key("type") {
            parameters.insert("type".into(), serde_json::Value::String("object".into()));
        }
        ToolSchema::new(self.name(), self.description()).parameters(parameters)
    }
}

/// A wrapper for a function-based tool.
pub struct FunctionTool {
    name: String,
    description: String,
    parameters: serde_json::Value,
    read_only: bool,
    handler: Box<dyn Fn(JsonMap, ToolExecutionContext) -> Result<ToolResultBlock> + Send + Sync>,
}

impl FunctionTool {
    /// Creates a new function tool.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        handler: Box<dyn Fn(JsonMap, ToolExecutionContext) -> Result<ToolResultBlock> + Send + Sync>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            read_only: false,
            handler,
        }
    }

    /// Marks this tool as read-only.
    pub fn read_only(mut self, v: bool) -> Self {
        self.read_only = v;
        self
    }
}

#[async_trait]
impl Tool for FunctionTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn is_read_only(&self) -> bool {
        self.read_only
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.parameters.clone()
    }
    async fn call(&self, args: JsonMap, ctx: ToolExecutionContext) -> Result<ToolResultBlock> {
        (self.handler)(args, ctx)
    }
}
