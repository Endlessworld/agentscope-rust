//! HayStack RAG integration.
//!
//! Ports the `haystack` submodule (`io.agentscope.core.rag.integration.haystack.*`, 10 classes)
//! onto `reqwest` + the shared [`crate::transport`] seam:
//!
//! * [`model`] — `HayStackResponse`, `HayStackDocument`, `SparseEmbedding`, `ByteStream` and the
//!   request-side `HayStackRequest`.
//! * [`config`] — `HayStackConfig` + builder and `FilterPolicy`.
//! * [`request`] — request-body construction (`query`, `top_k`, `filters`, ...).
//! * [`client`] — the HTTP client (`POST {baseUrl}`).
//! * [`converter`] — `HayStackDocument` → [`crate::model::Document`].
//! * [`knowledge`] — the [`crate::knowledge::Knowledge`] adapter (retrieval only).
//!
//! Haystack has no standard REST contract: Java targeted a *custom-built* Haystack RAG server
//! that answers with `{ "code": 0, "documents": [...] }` (or the `SentenceWindowRetriever`
//! shape with `context_windows` / `context_documents`). This port keeps that contract, including
//! the non-2xx tolerance for successful-to-parse bodies and the `code != successCode` API error.

mod client;
mod config;
mod converter;
mod knowledge;
mod model;
mod request;

pub use client::HayStackClient;
pub use config::{
    FilterPolicy, HayStackConfig, HayStackConfigBuilder, DEFAULT_MAX_RETRIES, DEFAULT_SUCCESS_CODE,
    DEFAULT_TIMEOUT, DEFAULT_TOP_K,
};
pub use converter::HayStackDocumentConverter;
pub use knowledge::{HayStackKnowledge, HayStackKnowledgeBuilder};
pub use model::{
    ByteStream, HayStackDocument, HayStackRequest, HayStackResponse, SparseEmbedding,
};
pub use request::build_retrieve_request_body;
