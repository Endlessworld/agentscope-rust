//! Provider-agnostic plumbing shared by every model provider adapter.
//!
//! The central abstraction is [`HttpTransport`]: providers never call
//! `reqwest` directly, they build an [`HttpRequestSpec`] and hand it to a
//! transport. Production code injects [`ReqwestTransport`]; tests inject the
//! in-memory [`MemoryTransport`] so the whole model layer stays fully offline.

pub mod transport;

pub use transport::{
    decode_stream_line, HttpRequestSpec, HttpTransport, JsonStream, MemoryTransport,
    ReqwestTransport, SharedTransport,
};
