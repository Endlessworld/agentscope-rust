//! # agentscope-extensions-model
//!
//! Provider adapters for [`agentscope_core::model::Model`], mirroring
//! `io.agentscope.extensions.model.*` from the Java SDK.
//!
//! ## Scope (per `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §2.2)
//! Core owns the [`Model`](agentscope_core::Model) trait, the
//! [`ModelRegistry`](agentscope_core::ModelRegistry) and the **OpenAI-compatible**
//! provider (`agentscope_core::model::openai`, built on `async-openai`). This
//! crate therefore **does not re-implement OpenAI**; it adds the providers whose
//! wire protocols differ from the OpenAI chat-completions shape.
//!
//! ## Offline-testability
//! Every provider talks to the network through [`common::HttpTransport`]. Tests
//! inject [`common::MemoryTransport`], so `cargo test -p agentscope-extensions-model`
//! performs **zero** real HTTP calls.
//!
//! See `docs/extensions/MODEL__GAP_TRACKER.md` for the authoritative gap list.

pub mod anthropic;
pub mod common;
pub mod dashscope;
pub mod gemini;
pub mod ollama;

pub use anthropic::{AnthropicChatModel, AnthropicModelProvider};
pub use common::{
    decode_stream_line, HttpRequestSpec, HttpTransport, JsonStream, MemoryTransport,
    ReqwestTransport, SharedTransport,
};
pub use dashscope::{
    DashScopeChatModel, DashScopeCredential, DashScopeModelProvider, EndpointType,
};
pub use gemini::{GeminiChatModel, GeminiModelProvider};
pub use ollama::{OllamaChatModel, OllamaModelProvider, OllamaOptions};
