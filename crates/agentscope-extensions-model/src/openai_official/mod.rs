//! OpenAI official Responses provider (`openai-official`).
//!
//! Mirrors the Java `agentscope-extensions-model-openai-official` module. The
//! provider id is **`openai-official`** (model-id prefix `openai-official:`),
//! which is deliberately distinct from the `openai-responses` id served by
//! [`crate::responses`]. Both speak the Responses protocol, but this module also
//! implements the official provider's server tools, multi-agent formatter,
//! `additionalBodyParams` whitelist and retry policy (408/409 included).
//!
//! ## Class mapping
//! | Java | Rust |
//! |------|------|
//! | `OpenAIOfficialModelProvider` | [`OpenAIOfficialModelProvider`] |
//! | `OpenAIOfficialConstants` | [`constants`] |
//! | `OpenAIOfficialModelException` + `OpenAIErrorTranslator` | [`error`] |
//! | `OpenAIOfficialCredential` | [`OpenAIOfficialCredential`] |
//! | `OpenAIResponsesChatModel` + `OpenAISdkClientFactory` | [`OpenAIResponsesChatModel`] (transport seam instead of the SDK) |
//! | `ResponsesRequestMapper` | [`request`] |
//! | `ResponsesResponseParser` + `ResponsesStreamingAssembler` + `ResponsesHelper` | [`parser`] |
//! | `ResponsesMultiAgentFormatter` | [`ResponsesMultiAgentFormatter`] |
//! | `OpenAIServerTool` + `ResponsesServerToolHelper` | [`server_tool`] |

pub mod constants;
pub mod credential;
pub mod error;
pub mod formatter;
pub mod model;
pub mod parser;
pub mod provider;
pub mod request;
pub mod server_tool;

pub use constants::{
    ADDITIONAL_BODY_PARAMS_WHITELIST, API_KEY_ENV, BASE_URL_ENV, DEFAULT_BASE_URL,
    MODEL_ID_PREFIX, PROVIDER_ID,
};
pub use credential::{OpenAIOfficialCredential, CREDENTIAL_TYPE};
pub use error::{
    is_retryable, translate, translate_body, translate_core, translate_optional_status,
    OpenAIOfficialModelError,
};
pub use formatter::ResponsesMultiAgentFormatter;
pub use model::{default_transport, OpenAIResponsesChatModel};
pub use parser::{extract_response_metadata, extract_usage, parse_response, ResponsesStreamingAssembler};
pub use provider::{default_openai_official_registry, OpenAIOfficialModelProvider};
pub use request::{
    map_history, map_request, resolve_data_block_image_url, resolve_image_url,
};
pub use server_tool::{
    HostedToolSearchPairing, OpenAIServerTool, OpenAIServerToolKind, ResponsesServerToolHelper,
    CODE_INTERPRETER, IMAGE_GENERATION, TOOL_SEARCH, WEB_SEARCH,
};
