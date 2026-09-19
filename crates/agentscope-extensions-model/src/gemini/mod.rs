//! Google Gemini (GenAI) adapter (`gemini:` models).
//!
//! Mirrors `io.agentscope.extensions.model.gemini`. Java delegates transport to
//! the official Google GenAI SDK; Rust has no such core dependency, so this
//! adapter speaks the REST wire protocol directly through the shared
//! [`crate::common::HttpTransport`] seam (see `MODEL__GAP_TRACKER.md` §4.2).
//!
//! ## Java → Rust file mapping
//!
//! | Java file | Rust file |
//! |---|---|
//! | `GeminiChatModel` | [`model::GeminiChatModel`] |
//! | `GeminiModelProvider` | [`provider::GeminiModelProvider`] |
//! | `credential.GeminiCredential` | [`credential::GeminiCredential`] |
//! | `formatter.GeminiChatFormatter` | [`converter::build_request`], [`converter::apply_options`] |
//! | `formatter.GeminiMessageConverter` | [`converter::to_gemini_contents`] |
//! | `formatter.GeminiToolsHelper` | [`converter::to_gemini_tools`], [`converter::to_tool_choice`] |
//! | `formatter.GeminiMediaConverter` | [`converter::media_part`], [`converter::data_part`] |
//! | `formatter.GeminiConversationMerger` | [`converter::merge_to_content`] |
//! | `formatter.GeminiMultiAgentFormatter` | [`converter::to_multi_agent_contents`] |
//! | `formatter.GeminiResponseParser` | [`parser`] |
//! | SDK wire types (`Content`, `Part`, `GenerateContentConfig`, `GenerateContentResponse`) | [`dto`] |

pub mod converter;
pub mod credential;
pub mod dto;
pub mod model;
pub mod parser;
pub mod provider;

pub use converter::{
    build_request, to_gemini_contents, to_gemini_tools, to_multi_agent_contents, to_tool_choice,
    DEFAULT_CONVERSATION_HISTORY_PROMPT,
};
pub use credential::{
    generate_content_path, stream_generate_content_path, GeminiCredential, API_KEY_ENV,
    API_KEY_HEADER, API_VERSION, CREDENTIAL_TYPE, DEFAULT_BASE_URL,
};
pub use dto::{
    GeminiCandidate, GeminiContent, GeminiFunctionCall, GeminiFunctionDeclaration,
    GeminiFunctionResponse, GeminiGenerationConfig, GeminiPart, GeminiRequest, GeminiResponse,
    GeminiThinkingConfig, GeminiTool, GeminiToolConfig, GeminiUsageMetadata,
};
pub use model::GeminiChatModel;
pub use parser::{parse_response, GeminiStreamIncrementer};
pub use provider::GeminiModelProvider;
