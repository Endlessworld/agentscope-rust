//! Alibaba Cloud Bailian (百炼) Knowledge Base integration.
//!
//! Ports the `bailian` submodule (`io.agentscope.core.rag.integration.bailian.*`, 7 classes)
//! onto `reqwest` + the shared [`crate::transport`] seam:
//!
//! * [`config`] — `BailianConfig` + builder, `RerankConfig`, `RewriteConfig`.
//! * [`query_history`] — `QueryHistoryEntry` for multi-turn query rewrite, plus the
//!   `Msg` → history conversion that Java implemented as `BailianKnowledge.convertToQueryHistory`.
//! * [`model`] — retrieve request/response DTOs. Request keys are `camelCase` and response
//!   keys are `PascalCase`, both verified against `bailian20231229` 2.8.2.
//! * [`request`] — `RetrieveRequest` assembly from the configuration + call site.
//! * [`signing`] — the ACS4-HMAC-SHA256 request signer used by the Java SDK's `AK`/`ROA`
//!   gateway (`com.aliyun.gateway.pop.Client`), reimplemented dependency-free on `hmac`/`sha2`.
//! * [`client`] — the HTTP client (`POST https://{endpoint}/{workspaceId}/index/retrieve`).
//! * [`converter`] — Bailian node → [`crate::model::Document`].
//! * [`knowledge`] — the [`crate::knowledge::Knowledge`] adapter (retrieval only).
//!
//! ## Design note: no Alibaba Cloud SDK
//!
//! Java wrapped `com.aliyun.bailian20231229` (Tea/OpenAPI). Introducing an Aliyun SDK into
//! the Rust workspace would pull a large, Java-centric dependency tree, so this port speaks
//! the same wire protocol directly with `reqwest` and reimplements only the one signing
//! algorithm the SDK actually uses (ACS4-HMAC-SHA256). The endpoint, path, API version/
//! action, request body keys, response keys and `Authorization` header format were all
//! reverse-engineered from the SDK jars and pinned by golden-vector unit tests.

mod client;
mod config;
mod converter;
mod knowledge;
mod model;
mod query_history;
mod request;
mod signing;

pub use client::BailianClient;
pub use config::{
    BailianConfig, BailianConfigBuilder, RerankConfig, RerankConfigBuilder, RewriteConfig,
    RewriteConfigBuilder, DEFAULT_ENDPOINT, DEFAULT_RERANK_MODEL, DEFAULT_REWRITE_MODEL,
};
pub use converter::BailianDocumentConverter;
pub use knowledge::{BailianKnowledge, BailianKnowledgeBuilder};
pub use model::{
    QueryHistorySpec, RerankSpec, RetrieveNode, RetrieveRequest, RetrieveResponseBody,
    RetrieveResponseData, RewriteSpec,
};
pub use query_history::{query_history_from_messages, QueryHistoryEntry, QueryRole};
pub use request::build_retrieve_request;
pub use signing::{
    build_authorization, hashed_payload, percent_encode, resolve_region, short_date, signing_key,
    SignInput, API_ACTION, API_VERSION, PRODUCT, SIGNATURE_ALGORITHM,
};

/// The Bailian retrieve path appended to the workspace id (`/{workspaceId}/index/retrieve`).
pub const RETRIEVE_PATH: &str = "/index/retrieve";
