//! OpenAI-compatible Chat Completions web adapter, mirroring
//! `agentscope-extensions-chat-completions-web` (`io.agentscope.core.chat.completions`).
//!
//! Sub-modules:
//! * [`model`] - OpenAI wire DTOs (request/response/chunk/message/tool).
//! * [`converter`] - `ChatMessage` <-> core `Msg`, `OpenAITool` -> `ToolSchema`.
//! * [`builder`] - non-streaming response construction from core [`Msg`] values.
//! * [`streaming`] - streaming adaptation of agent events into chunks.

pub mod builder;
pub mod converter;
pub mod model;
pub mod streaming;

pub use builder::ChatCompletionsResponseBuilder;
pub use converter::{ChatMessageConverter, OpenAIToolConverter};
pub use model::{
    ChatChoice, ChatCompletionsChunk, ChatCompletionsRequest, ChatCompletionsResponse, ChatMessage,
    FunctionCall, OpenAITool, OpenAIToolFunction, ToolCall, Usage,
};
pub use streaming::{AgentStreamEvent, AgentStreamEventType, ChatCompletionsStreamingAdapter};
