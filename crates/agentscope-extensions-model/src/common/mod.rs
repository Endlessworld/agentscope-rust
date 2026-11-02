//! Provider-agnostic plumbing shared by every model provider adapter.
//!
//! The central abstraction is [`HttpTransport`]: providers never call
//! `reqwest` directly, they build an [`HttpRequestSpec`] and hand it to a
//! transport. Production code injects [`ReqwestTransport`]; tests inject the
//! in-memory [`MemoryTransport`] so the whole model layer stays fully offline.
//!
//! Around that seam this module layers the cross-provider concerns every
//! adapter needs: structured error normalisation ([`error::ModelError`]),
//! usage merging ([`usage::merge_non_zero`]), model capabilities
//! ([`capabilities::resolve_capabilities`]), conversion diagnostics
//! ([`diagnostics`]), media resolution ([`media`]), model-name mapping
//! ([`model_map`]) and the provider-native tool-search contract
//! ([`tool_search`]).

pub mod base_url;
pub mod capabilities;
pub mod diagnostics;
pub mod error;
pub mod media;
pub mod model_map;
pub mod multi_agent;
pub mod tool_search;
pub mod transport;
pub mod usage;

pub use base_url::{resolve_base_url, resolve_base_url_with};
pub use capabilities::{
    clamp_max_tokens, resolve_capabilities, resolve_capabilities_with, ModelCapabilities,
};
pub use diagnostics::{
    clear_diagnostic_sink, set_diagnostic_sink, ConversionDiagnostic, DiagnosticSink, Diagnostics,
    Severity,
};
pub use error::{ErrorKind, ModelError};
pub use media::{
    media_resolver, remote_media_cache, resolve_media, set_media_resolver, DefaultMediaResolver,
    MediaResolver, RemoteMediaCache, ResolvedMedia,
};
pub use model_map::{strip_reasoning_suffix, ModelMap, ModelMapError, ReasoningHint};
pub use multi_agent::{
    agent_name, group_messages, is_tool_related, merge_agent_group, merge_multi_agent_history,
    take_bool_option, take_multi_agent_prompt, take_option, text_only, GroupType,
    DEFAULT_CONVERSATION_HISTORY_PROMPT, HISTORY_END_TAG, HISTORY_START_TAG,
    MULTI_AGENT_FORMATTER_KEY,
};
pub use tool_search::{
    is_tool_search_schema, loaded_tool_names, loaded_tool_schemas, tool_search_schema,
    ToolSearchMode, METADATA_CLIENT_TOOL_SEARCH_CALL, METADATA_MODE, METADATA_TOOLS,
    TOOL_SEARCH_TOOL_NAME,
};
pub use transport::{
    decode_stream_line, HttpRequestSpec, HttpTransport, JsonStream, MemoryTransport,
    ReqwestTransport, SharedTransport, TransportConfig,
};
pub use usage::{merge_into, merge_non_zero};
