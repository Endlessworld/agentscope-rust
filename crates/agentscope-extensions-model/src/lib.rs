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
//!
//! ## Production assembly
//!
//! [`assembly`] is the protocol-driven construction point for hosts that are
//! configured with a wire protocol rather than a vendor. It makes the
//! capability-rich adapters reachable in production:
//! [`assembly::build_model_for_protocol`] with
//! [`assembly::ProtocolImplementations::full_capability`] selects the Anthropic
//! transport-seam Messages adapter (`cache_control` / thinking / server tools /
//! deferred tool search) and the OpenAI-official Responses adapter (server
//! tools / `tool_search` / `prompt_cache`), instead of the SDK-backed ones.
//! [`assembly::full_registry`] does the same for
//! [`ModelRegistry`](agentscope_core::ModelRegistry) based hosts. See
//! `docs/extensions/MODEL__GAP_TRACKER.md` MODEL-01.
//!
//! ## Registry resolution semantics
//! [`ModelRegistry`](agentscope_core::ModelRegistry) belongs to core and is
//! deliberately **uncached**: `register` takes `&mut self` and replaces the
//! provider registered under the same `provider_id`, while `resolve` builds a
//! fresh model instance on every call. Java `9e716cb27` hardened the Java
//! registry's *resolution cache* (evict on `registerFactory`, share one
//! creation across concurrent first callers, never cache failures); neither
//! defect can occur here because there is no cache in the first place. The
//! invariants a future cache would have to preserve are pinned by
//! `tests/registry_semantics.rs`; moving to a cached registry is a **core-side**
//! change, recorded in `docs/extensions/MODEL__GAP_TRACKER.md` §4.2 #11.

pub mod assembly;
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

pub use assembly::{
    build_messages_wire_model, build_model_for_protocol, build_model_for_protocol_with_transport,
    build_openai_official_model, full_registry, implementation_for, MessagesImplementation,
    ModelImplementation, ProtocolImplementations, ResponsesImplementation,
};
pub use anthropic::AnthropicServerTool;
pub use anthropic::{AnthropicChatModel, AnthropicModelProvider};
pub use anthropic_async::{
    default_anthropic_registry, AnthropicMessagesModel, AnthropicMessagesProvider,
};
pub use common::{
    decode_stream_line, media_resolver, merge_non_zero, remote_media_cache, resolve_capabilities,
    resolve_media, set_diagnostic_sink, set_media_resolver, strip_reasoning_suffix,
    tool_search_schema, ConversionDiagnostic, DefaultMediaResolver, Diagnostics, ErrorKind,
    HttpRequestSpec, HttpTransport, JsonStream, MediaResolver, MemoryTransport, ModelCapabilities,
    ModelError, ModelMap, RemoteMediaCache, ReqwestTransport, ResolvedMedia, Severity,
    SharedTransport, ToolSearchMode, TransportConfig,
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
