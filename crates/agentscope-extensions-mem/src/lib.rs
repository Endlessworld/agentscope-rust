//! # agentscope-extensions-mem
//!
//! Rust port of the AgentScope Java `agentscope-extensions-mem` aggregate module,
//! covering three vendor long-term-memory backends:
//!
//! - [`mem0`] — `io.agentscope.core.memory.mem0` (Mem0 platform / self-hosted API)
//! - [`bailian`] — `io.agentscope.core.memory.bailian` (Alibaba Cloud Bailian Memory)
//! - [`reme`] — `io.agentscope.core.memory.reme` (ReMe personal-memory service)
//!
//! ## Design decision (see `docs/extensions/MEM__GAP_TRACKER.md` §4.2)
//!
//! The Java sources implement `io.agentscope.core.memory.LongTermMemory`, which is
//! `@Deprecated(forRemoval = true, since = "2.0.0")`; the Java docs state that
//! long-term memory has been removed in favour of `AgentState::getContext()`.
//! The Rust `agentscope_core::Memory` trait is a *conversation-buffer* abstraction
//! (`add_message` / `get_messages` / `delete_message` / `clear` / `save_to` /
//! `load_from`) and is already implemented by `InMemoryMemory` and
//! `StateBackedMemory`; it is **not** semantically equivalent to `LongTermMemory`
//! (`record(&[Msg])` / `retrieve(&Msg) -> String`).
//!
//! We therefore **do not** re-create the deprecated `LongTermMemory` trait. Each
//! vendor adapter exposes inherent `record` / `retrieve` methods (plus a
//! `retrieve_msgs` bridge on the Mem0 adapter that maps hits back to core [`Msg`]s),
//! preserving the Java behaviour without introducing a parallel abstraction.
//!
//! ## Construction is validated
//!
//! Every client constructor (`Mem0Client::new` / `Mem0Client::with_api_type` /
//! `Mem0Client::with_transport`, `ReMeClient::new` / `with_timeout` / `with_transport`,
//! `BailianMemoryClient::*`) rejects an empty or blank base URL at construction
//! time, mirroring the Java adapter builders (`"apiBaseUrl is required"` for
//! Mem0 / ReMe, `"apiBaseUrl cannot be null or blank"` for Bailian). The
//! `with_transport` constructors return [`Result`] for the same reason, so no
//! construction path can yield a host-less request URL.
//!
//! ## Offline by default
//!
//! All clients depend on the [`HttpTransport`] seam, so assembly can inject
//! [`http::ReqwestTransport`] in production or an in-memory mock in tests. The test
//! suite performs no network I/O.

pub mod bailian;
pub mod error;
pub mod http;
pub mod mem0;
pub mod reme;

pub use bailian::{
    BailianAddRequest, BailianAddResponse, BailianLongTermMemory, BailianLongTermMemoryConfig,
    BailianMemoryClient, BailianMemoryNode, BailianMessage, BailianSearchRequest,
    BailianSearchResponse, ADD_MEMORY_ENDPOINT,
};
pub use error::{MemError, Result};
pub use http::{HttpTransport, ReqwestTransport};
pub use mem0::{
    Mem0AddRequest, Mem0AddResponse, Mem0ApiType, Mem0Client, Mem0LongTermMemory,
    Mem0LongTermMemoryConfig, Mem0Message, Mem0SearchRequest, Mem0SearchResponse, Mem0SearchResult,
    PLATFORM_MEMORIES_ENDPOINT, PLATFORM_SEARCH_ENDPOINT, SELF_HOSTED_MEMORIES_ENDPOINT,
    SELF_HOSTED_SEARCH_ENDPOINT,
};
pub use reme::{
    ReMeAddRequest, ReMeAddResponse, ReMeClient, ReMeLongTermMemory, ReMeMemoryItem, ReMeMessage,
    ReMeSearchRequest, ReMeSearchResponse, ReMeTrajectory, RETRIEVE_ENDPOINT, SUMMARY_ENDPOINT,
};
