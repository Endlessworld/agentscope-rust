//! Dify Knowledge Base integration.
//!
//! Ports the `rag-dify` submodule: configuration, retrieval-mode enum, metadata filters,
//! response DTOs, request-body construction, the API client and the `Knowledge` adapter.
//!
//! `reqwest` is used for real HTTP, but the client depends on the [`DifyTransport`] trait so
//! tests (and callers) can inject a mock and never touch the network.

mod client;
mod config;
mod converter;
mod knowledge;
mod model;
mod request;

pub use client::{DifyRAGClient, DifyTransport, HttpDifyTransport};
pub use config::{
    DifyRAGConfig, DifyRAGConfigBuilder, MetadataFilter, MetadataFilterCondition, RerankConfig,
    RetrievalMode,
};
pub use converter::DifyDocumentConverter;
pub use knowledge::DifyKnowledge;
pub use model::{DifyResponse, DocumentInfo, Query, Record, Segment};
pub use request::build_retrieve_request_body;
