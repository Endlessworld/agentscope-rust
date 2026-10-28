//! DashScope provider adapter (`qwen-*`, `dashscope:` models).
//!
//! Mirrors `io.agentscope.extensions.model.dashscope`. DashScope uses a nested
//! `{model, input:{messages}, parameters:{...}}` request shape and an
//! `{request_id, output:{choices}, usage}` response shape, both distinct from
//! the OpenAI protocol, so this adapter is implemented from scratch.

pub mod converter;
pub mod credential;
pub mod crypto;
pub mod dto;
pub mod encryption;
pub mod model;
pub mod parser;
pub mod provider;

pub use converter::{
    apply_automatic_cache_control, ephemeral_cache_control, to_dashscope_messages_with_options,
    MAX_CACHE_MARKERS,
};
pub use converter::{
    build_request, is_multimodal_model, merge_conversation, merge_multimodal_conversation,
    role_str, to_dashscope_messages, to_dashscope_tools, to_multi_agent_messages,
    to_tool_choice_value, DEFAULT_BASE_URL, MULTIMODAL_GENERATION_ENDPOINT,
    TEXT_GENERATION_ENDPOINT,
};

/// Default conversation-history prompt for the multi-agent formatter.
///
/// Re-exported from [`crate::common::multi_agent`] so the DashScope and Ollama
/// adapters provably share the Java formatters' prompt string.
pub use crate::common::multi_agent::DEFAULT_CONVERSATION_HISTORY_PROMPT;
pub use credential::{DashScopeCredential, EndpointType, CREDENTIAL_TYPE};
pub use crypto::AesRsaCryptoProvider;
pub use dto::{
    CacheControl, DashScopeChoice, DashScopeContent, DashScopeContentPart, DashScopeFunction,
    DashScopeInput, DashScopeMessage, DashScopeOutput, DashScopeParameters,
    DashScopePublicKeyResponse, DashScopeRequest, DashScopeResponse, DashScopeSearchOptions,
    DashScopeTool, DashScopeToolCall, DashScopeToolFunction, DashScopeUsage, PublicKeyData,
};
pub use encryption::{
    decrypt_response_body, encrypt_request_body, encryption_header_value, DashScopeCryptoProvider,
    EncryptionConfig, EncryptionContext, UnavailableCryptoProvider, AES_KEY_SIZE,
    ENCRYPTION_KEY_HEADER, GCM_IV_LENGTH, GCM_TAG_LENGTH_BITS,
};
pub use model::{DashScopeChatModel, PUBLIC_KEYS_ENDPOINT};
pub use parser::{parse_response, DashScopeStreamIncrementer, FRAGMENT_PLACEHOLDER};
pub use provider::DashScopeModelProvider;
