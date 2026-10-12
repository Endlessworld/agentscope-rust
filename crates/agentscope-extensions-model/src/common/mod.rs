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
//! ([`diagnostics`]), media resolution ([`media`]) and model-name mapping
//! ([`model_map`]).

pub mod capabilities;
pub mod diagnostics;
pub mod error;
pub mod media;
pub mod model_map;
pub mod transport;
pub mod usage;

pub use capabilities::{resolve_capabilities, ModelCapabilities};
pub use diagnostics::{ConversionDiagnostic, Diagnostics, Severity};
pub use error::{ErrorKind, ModelError};
pub use media::{
    media_resolver, resolve_media, set_media_resolver, DefaultMediaResolver, MediaResolver,
    ResolvedMedia,
};
pub use model_map::{strip_reasoning_suffix, ModelMap, ModelMapError, ReasoningHint};
pub use transport::{
    decode_stream_line, HttpRequestSpec, HttpTransport, JsonStream, MemoryTransport,
    ReqwestTransport, SharedTransport, TransportConfig,
};
pub use usage::{merge_into, merge_non_zero};
