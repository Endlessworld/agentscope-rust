//! Model abstraction for chat completion, mirroring the Java `Model` trait.
use futures::stream::BoxStream;

use crate::message::Msg;

use super::chat_response::ChatResponse;
use super::generate_options::GenerateOptions;
use super::tool_schema::ToolSchema;

/// A language model that can stream chat completions.
#[async_trait::async_trait]
pub trait Model: Send + Sync {
    /// Streams chat completion responses for the given messages, tools and options.
    fn stream(
        &self,
        messages: Vec<Msg>,
        tools: Vec<ToolSchema>,
        options: GenerateOptions,
    ) -> BoxStream<'_, crate::error::Result<ChatResponse>>;

    /// The model name, for logging and identification.
    fn model_name(&self) -> &str;

    /// Whether this model supports native structured output.
    fn supports_native_structured_output(&self) -> bool {
        false
    }

    /// Whether this model handles `response_format` alongside tools correctly.
    fn supports_native_structured_output_with_tools(&self) -> bool {
        self.supports_native_structured_output()
    }

    /// Whether this model honours a request-level `ToolChoice::Specific` constraint.
    /// Defaults to `false`; providers that support forcing a named tool override this.
    fn supports_tool_choice_specific(&self) -> bool {
        false
    }

    /// The model's context window size in tokens, or 0 if unknown.
    fn context_window_size(&self) -> usize {
        0
    }
}

/// Shared capability flags for models, mirroring `ChatModelBase`.
#[derive(Debug, Clone, Default)]
pub struct ModelCapabilities {
    /// Total context window size in tokens (input + output).
    pub context_window_size: usize,
    /// Whether the model supports native structured output.
    pub native_structured_output: bool,
    /// Whether the model supports native structured output alongside tools.
    pub native_structured_output_with_tools: bool,
}

impl ModelCapabilities {
    /// Creates a new empty capability set.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn context_window_size(mut self, v: usize) -> Self {
        self.context_window_size = v;
        self
    }

    pub fn native_structured_output(mut self, v: bool) -> Self {
        self.native_structured_output = v;
        self
    }

    pub fn native_structured_output_with_tools(mut self, v: bool) -> Self {
        self.native_structured_output_with_tools = v;
        self
    }
}
