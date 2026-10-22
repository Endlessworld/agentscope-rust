//! Shared constants for the `openai-official` provider.
//!
//! Mirrors `OpenAIOfficialConstants` from the Java module: the provider id, the
//! model-id prefix, the `additionalBodyParams` whitelist and the metadata
//! namespace (`openai.*`) used for history replay.

/// Provider id used in `ModelError`, SPI registration and `provider:model` ids.
pub const PROVIDER_ID: &str = "openai-official";

/// Model id prefix accepted by [`crate::openai_official::OpenAIOfficialModelProvider`].
pub const MODEL_ID_PREFIX: &str = "openai-official:";

/// Default OpenAI API base URL (the official SDK default).
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Environment variable consulted when no explicit API key is supplied.
pub const API_KEY_ENV: &str = "OPENAI_API_KEY";

/// Environment variable consulted when no explicit base URL is supplied.
pub const BASE_URL_ENV: &str = "OPENAI_BASE_URL";

/// Whitelist of keys allowed in `GenerateOptions::additional_body_params`.
///
/// Any other key is rejected fail-fast during request mapping, mirroring
/// `OpenAIOfficialConstants.ADDITIONAL_BODY_PARAMS_WHITELIST`.
pub const ADDITIONAL_BODY_PARAMS_WHITELIST: &[&str] = &[
    "max_tool_calls",
    "prompt_cache_key",
    "prompt_cache_options",
    "previous_response_id",
    "service_tier",
    "safety_identifier",
    "store",
    "reasoning.summary",
    "reasoning.context",
    "reasoning.mode",
];

// ── Response-level metadata ────────────────────────────────────────
pub const MD_RESPONSE_ID: &str = "openai.response.id";
pub const MD_RESPONSE_STATUS: &str = "openai.response.status";
pub const MD_RESPONSE_CREATED_AT: &str = "openai.response.created_at";
pub const MD_RESPONSE_COMPLETED_AT: &str = "openai.response.completed_at";
pub const MD_RESPONSE_INCOMPLETE_REASON: &str = "openai.response.incomplete_reason";
pub const MD_RESPONSE_ERROR: &str = "openai.response.error";
pub const MD_RESPONSE_CITATIONS: &str = "openai.response.citations";

// ── Reasoning-level metadata (history replay) ──────────────────────
pub const MD_REASONING_ENCRYPTED_CONTENT: &str = "openai.reasoning.encrypted_content";
pub const MD_REASONING_TEXT: &str = "openai.reasoning.text";

// ── Function-call-level metadata (history replay) ──────────────────
pub const MD_FUNCTION_CALL_NAMESPACE: &str = "openai.functionCall.namespace";

// ── Server-tool metadata (history replay) ──────────────────────────
/// Raw Responses output item JSON, echoed back on later turns.
pub const MD_RESPONSES_SERVER_TOOL_ITEM: &str = "openai.serverToolItem";
/// Code Interpreter container id.
pub const MD_CODE_INTERPRETER_CONTAINER_ID: &str = "openai.codeInterpreter.containerId";

/// Whether an HTTP status is retryable for the official provider.
///
/// Mirrors `OpenAIOfficialModelException.isRetryableHttpStatus`: 408 and 409 are
/// retryable in addition to the core 429/5xx set (the official SDK retries
/// `{408, 409, 429, >=500}`).
pub fn is_retryable_http_status(status: u16) -> bool {
    matches!(status, 408 | 409 | 429 | 500..=599)
}

/// Whether an `additionalBodyParams` key is whitelisted.
pub fn is_whitelisted_body_param(key: &str) -> bool {
    ADDITIONAL_BODY_PARAMS_WHITELIST.contains(&key)
}
