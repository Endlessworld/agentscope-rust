//! # agentscope-extensions-rag
//!
//! Rust port of the Java `agentscope-extensions-rag` module (submodules `bailian`, `dify`,
//! `haystack`, `ragflow`, `simple`) plus the small `io.agentscope.core.rag` / `embedding`
//! surface that the extensions implement. See `docs/extensions/RAG__GAP_TRACKER.md` for the
//! module tracker and the remaining gaps.
//!
//! ## What is here
//!
//! * [`model`] — [`Document`](model::Document), [`DocumentMetadata`](model::DocumentMetadata)
//!   and [`RetrieveConfig`](model::RetrieveConfig) (ported from Java core
//!   `io.agentscope.core.rag.model`).
//! * [`knowledge`] — the [`Knowledge`](knowledge::Knowledge) trait and the shared
//!   filter/sort/limit helper used by every backend.
//! * [`store`] — [`VdbStore`](store::VdbStore), [`InMemoryStore`](store::InMemoryStore) and
//!   cosine/Euclidean distance helpers.
//! * [`embedding`] — the [`EmbeddingModel`](embedding::EmbeddingModel) trait plus a
//!   deterministic, offline [`HashEmbeddingModel`](embedding::HashEmbeddingModel).
//! * [`reader`] — [`Reader`](reader::Reader), [`TextReader`](reader::TextReader) and the
//!   character/paragraph/token chunking algorithms.
//! * [`message`] — mapping retrieved documents onto [`agentscope_core::message::Msg`],
//!   covering both the agentic-tool format and the generic-hook `<retrieved_knowledge>`
//!   injection.
//! * [`retry`] — the exponential-backoff retry helper used by HTTP backends.
//! * [`simple`] — [`SimpleKnowledge`](simple::SimpleKnowledge), a local, fully offline
//!   embedding + in-memory vector store knowledge base.
//! * [`dify`] — the Dify Knowledge Base client ([`DifyRAGClient`](dify::DifyRAGClient),
//!   [`DifyDocumentConverter`](dify::DifyDocumentConverter),
//!   [`DifyKnowledge`](dify::DifyKnowledge)).
//! * [`ragflow`] — the RAGFlow retrieval client + `Knowledge` adapter.
//! * [`bailian`] — the Alibaba Cloud Bailian client
//!   ([`BailianClient`](bailian::BailianClient),
//!   [`BailianDocumentConverter`](bailian::BailianDocumentConverter),
//!   [`BailianKnowledge`](bailian::BailianKnowledge)), including the dependency-free
//!   ACS4-HMAC-SHA256 request signer the Java SDK used to apply.
//! * [`haystack`] — the custom Haystack RAG client
//!   ([`HayStackClient`](haystack::HayStackClient),
//!   [`HayStackDocumentConverter`](haystack::HayStackDocumentConverter),
//!   [`HayStackKnowledge`](haystack::HayStackKnowledge)).
//!
//! ## Design notes
//!
//! * The Java core `rag` package is `@Deprecated(forRemoval)` and has no Rust counterpart in
//!   `agentscope-core`, so the model/traits live in this crate (see
//!   `docs/extensions/RAG__GAP_TRACKER.md` §4.2).
//! * All public APIs are `async` (`async_trait` / `tokio`) where Java used Reactor `Mono`.
//! * Remote backends talk HTTP through an injectable transport trait, so the whole test suite
//!   runs offline with mocks.

pub mod bailian;
pub mod dify;
pub mod embedding;
pub mod error;
pub mod hash;
pub mod haystack;
pub mod knowledge;
pub mod message;
pub mod model;
pub mod ragflow;
pub mod reader;
pub mod retry;
pub mod simple;
pub mod store;
pub mod transport;

pub use embedding::{EmbeddingModel, HashEmbeddingModel, DEFAULT_DIMENSIONS};
pub use error::{RagError, Result};
pub use knowledge::{filter_and_rank, Knowledge};
pub use message::{
    build_retrieved_knowledge_msg, extract_query_from_messages, format_documents_for_tool,
    RETRIEVED_KNOWLEDGE_CLOSE, RETRIEVED_KNOWLEDGE_OPEN,
};
pub use model::{Document, DocumentMetadata, RetrieveConfig};
pub use reader::{chunk_text, Reader, ReaderInput, SplitStrategy, TextReader};
pub use simple::{in_memory_knowledge, EmptyKnowledge, SimpleKnowledge};
pub use store::{
    cosine_similarity, euclidean_distance, squared_euclidean_distance, InMemoryStore, SearchQuery,
    VdbStore,
};

