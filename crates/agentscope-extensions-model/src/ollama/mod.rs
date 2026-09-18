//! Ollama provider adapter (`ollama:` models).
//!
//! Mirrors `io.agentscope.extensions.model.ollama`. Ollama exposes a local
//! HTTP API (`/api/chat`) that speaks NDJSON streaming, so this adapter does
//! **not** reuse the core OpenAI provider.

pub mod converter;
pub mod dto;
pub mod model;
pub mod options;
pub mod parser;
pub mod provider;

pub use converter::{build_request, to_ollama_messages, to_ollama_tools};
pub use dto::{
    OllamaFunction, OllamaMessage, OllamaRequest, OllamaResponse, OllamaTool, OllamaToolCall,
    OllamaToolFunction,
};
pub use model::OllamaChatModel;
pub use options::OllamaOptions;
pub use parser::{parse_response, OllamaStreamIncrementer};
pub use provider::OllamaModelProvider;
