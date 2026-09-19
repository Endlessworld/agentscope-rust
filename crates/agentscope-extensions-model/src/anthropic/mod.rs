//! Anthropic Messages API adapter (`anthropic:` models).
//!
//! Mirrors `io.agentscope.extensions.model.anthropic`. Java delegates transport
//! to the official Anthropic SDK; Rust has no such core dependency, so this
//! adapter speaks the wire protocol directly through the shared
//! [`crate::common::HttpTransport`] seam (see `MODEL__GAP_TRACKER.md` §4.2).
//!
//! ## Java → Rust file mapping
//!
//! | Java file | Rust file |
//! |---|---|
//! | `AnthropicChatModel` | [`model::AnthropicChatModel`] |
//! | `AnthropicModelProvider` | [`provider::AnthropicModelProvider`] |
//! | `credential.AnthropicCredential` | [`credential::AnthropicCredential`] |
//! | `formatter.AnthropicBaseFormatter` / `AnthropicChatFormatter` | [`converter::build_request`] / [`converter::ChatFormatOptions`] |
//! | `formatter.AnthropicMessageConverter` | [`converter::to_anthropic_messages`] |
//! | `formatter.AnthropicToolsHelper` | [`converter::to_anthropic_tools`], [`converter::to_tool_choice`] |
//! | `formatter.AnthropicMediaConverter` | [`converter::image_source`], [`converter::image_content_block`] |
//! | `formatter.AnthropicConversationMerger` | [`converter::merge_conversation`] |
//! | `formatter.AnthropicMultiAgentFormatter` | [`converter::to_multi_agent_messages`] |
//! | `formatter.AnthropicResponseParser` | [`parser`] |
//! | SDK wire types (`MessageCreateParams`, `MessageParam`, `Message`) | [`dto`] |

pub mod converter;
pub mod credential;
pub mod dto;
pub mod model;
pub mod parser;
pub mod provider;

pub use converter::{
    build_request, to_anthropic_messages, to_anthropic_tools, to_multi_agent_messages,
    to_tool_choice, ChatFormatOptions, DEFAULT_CONVERSATION_HISTORY_PROMPT,
};
pub use credential::{
    AnthropicAuthMode, AnthropicCredential, ANTHROPIC_VERSION, CREDENTIAL_TYPE, DEFAULT_BASE_URL,
    MESSAGES_ENDPOINT,
};
pub use dto::{
    AnthropicContentBlock, AnthropicMessage, AnthropicMessageResponse, AnthropicRequest,
    AnthropicStreamEvent, AnthropicTool, AnthropicToolChoice, AnthropicUsage,
};
pub use model::AnthropicChatModel;
pub use parser::{parse_message, AnthropicStreamIncrementer, FRAGMENT_PLACEHOLDER};
pub use provider::AnthropicModelProvider;
