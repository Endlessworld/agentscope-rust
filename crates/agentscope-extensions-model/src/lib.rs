//! # agentscope-extensions-model
//!
//! Provider adapters for [`agentscope_core::model::Model`], mirroring
//! `io.agentscope.extensions.model.*` from the Java SDK.
//!
//! ## Scope (per `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §2.2)
//! Core owns the [`Model`](agentscope_core::Model) trait, the
//! [`ModelRegistry`](agentscope_core::ModelRegistry) and the shared provider
//! SPI. **All concrete model implementations live here**, including the
//! OpenAI-compatible provider (`openai`, on `async-openai`), the OpenAI
//! Responses provider (`responses`) and the async-anthropic-backed Messages
//! provider (`anthropic_async`, alongside the transport-seam `anthropic`).
//!
//! ## Offline-testability
//! Every provider talks to the network through [`common::HttpTransport`]. Tests
//! inject [`common::MemoryTransport`], so `cargo test -p agentscope-extensions-model`
//! performs **zero** real HTTP calls.
//!
//! See `docs/extensions/MODEL__GAP_TRACKER.md` for the authoritative gap list.

pub mod anthropic;
pub mod anthropic_async;
pub mod common;
pub mod dashscope;
pub mod gemini;
pub mod ollama;
pub mod openai;
pub mod openai_official;
pub mod relay;
pub mod responses;

pub use anthropic::AnthropicServerTool;
pub use anthropic::{AnthropicChatModel, AnthropicModelProvider};
pub use anthropic_async::{
    default_anthropic_registry, AnthropicMessagesModel, AnthropicMessagesProvider,
};
pub use common::{
    decode_stream_line, merge_non_zero, resolve_capabilities, resolve_media, set_media_resolver,
    strip_reasoning_suffix, ConversionDiagnostic, DefaultMediaResolver, Diagnostics, ErrorKind,
    HttpRequestSpec, HttpTransport, JsonStream, MediaResolver, MemoryTransport, ModelCapabilities,
    ModelError, ModelMap, ReqwestTransport, ResolvedMedia, SharedTransport, TransportConfig,
};
pub use dashscope::{
    DashScopeChatModel, DashScopeCredential, DashScopeCryptoProvider, DashScopeModelProvider,
    EncryptionConfig, EncryptionContext, EndpointType, UnavailableCryptoProvider,
};
pub use gemini::{GeminiChatModel, GeminiModelProvider};
pub use ollama::{OllamaChatModel, OllamaModelProvider, OllamaOptions};
pub use openai::{default_registry, OpenAiChatModel, OpenAiProvider};
pub use openai_official::{
    default_openai_official_registry, OpenAIOfficialCredential, OpenAIOfficialModelProvider,
    OpenAIResponsesChatModel, OpenAIServerTool, OpenAIServerToolKind, ProxyConfig, ProxyType,
    ResponsesMultiAgentFormatter, ResponsesServerToolHelper,
};
pub use relay::{
    encode_error, encode_response, find_route, stream_terminator, ConverterQuality, ConverterSpec,
    RelayFormat, BUILTIN_CONVERTERS,
};
pub use responses::{default_responses_registry, OpenAiResponsesModel, OpenAiResponsesProvider};
