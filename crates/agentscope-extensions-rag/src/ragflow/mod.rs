//! RAGFlow Knowledge Base integration.
//!
//! Ports the `ragflow` submodule (`io.agentscope.core.rag.integration.ragflow.*`, 9 classes)
//! onto `reqwest` + the shared [`crate::transport`] seam:
//!
//! * [`config`] — `RAGFlowConfig` + builder (dataset/document ids, retrieval knobs, retries).
//! * [`model`] — response DTOs, including the `data: false | null | object` custom
//!   deserialization that Java implemented as `ResponseDataDeserializer`.
//! * [`request`] — `POST /api/v1/retrieval` body construction.
//! * [`converter`] — `RAGFlowChunk` → [`crate::model::Document`].
//! * [`client`] — the HTTP client (error mapping + retries).
//! * [`knowledge`] — the [`crate::knowledge::Knowledge`] adapter (retrieval only).

mod client;
mod config;
mod converter;
mod knowledge;
mod model;
mod request;

pub use client::RAGFlowClient;
pub use config::{RAGFlowConfig, RAGFlowConfigBuilder};
pub use converter::RAGFlowDocumentConverter;
pub use knowledge::RAGFlowKnowledge;
pub use model::{DocAgg, RAGFlowChunk, RAGFlowResponse, ResponseData};
pub use request::build_retrieval_request_body;

/// The RAGFlow retrieval endpoint path appended to the configured base URL.
pub const RETRIEVAL_PATH: &str = "/api/v1/retrieval";
