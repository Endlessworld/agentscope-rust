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
//! ## Framework assembly: `MemLongTermMemoryMiddleware` (MEM-01)
//!
//! The Java hook (`StaticLongTermMemoryHook`) and its `ReActAgent.Builder.longTermMemory*`
//! setters are deprecated with the note "reimplement as a `MiddlewareBase` if required",
//! and without such a seam this crate had no production wiring point at all — every host
//! had to hand-write the retrieve/record call sites. [`middleware`] provides that seam:
//!
//! ```text
//! let memory = Arc::new(Mem0LongTermMemory::new(client, config)?);
//! let mut chain = MiddlewareChain::new();
//! chain.add(Box::new(MemLongTermMemoryMiddleware::new(memory)));
//! let agent = HarnessAgent::builder().model(model).middleware(chain).build();
//! ```
//!
//! [`MemLongTermMemoryMiddleware`] retrieves on `on_reasoning` (appending a
//! `long_term_memory` USER message — the Java `PreReasoningEvent` behaviour) and records
//! the finished conversation on `on_agent` (the Java `PostCallEvent` behaviour), using
//! the never-fail path in both directions so a memory outage cannot abort a turn. The
//! [`LongTermMemoryBackend`] trait is the narrow seam over the three adapters; it is not
//! a revival of the removed `LongTermMemory` interface.
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
//! ## Error semantics: strict default, `*_lossy` for Java parity
//!
//! The Java adapters swallow failures with Reactor operators. The Rust port keeps
//! that behaviour **explicitly opt-in** instead of baked into the only entry point:
//!
//! | Java | Rust | Observable behaviour |
//! |---|---|---|
//! | `Mem0LongTermMemory.retrieve` → `.onErrorReturn("")` | [`Mem0LongTermMemory::retrieve_lossy`] | never fails; logs `tracing::warn!` and returns `""` |
//! | `Mem0LongTermMemory.record` (`client.add(...).then()`) | [`Mem0LongTermMemory::record`] | error propagates (Java does not swallow here) |
//! | `BailianLongTermMemory.record` → `.onErrorComplete()` | [`BailianLongTermMemory::record_lossy`] | never fails; logs `tracing::warn!` |
//! | `BailianLongTermMemory.retrieve` → `.onErrorReturn("")` | [`BailianLongTermMemory::retrieve_lossy`] | never fails; logs `tracing::warn!` and returns `""` |
//! | `ReMeLongTermMemory.retrieve` → `.onErrorReturn("")` | [`ReMeLongTermMemory::retrieve_lossy`] | never fails; logs `tracing::warn!` and returns `""` |
//! | `ReMeLongTermMemory.record` (`client.add(...).then()`) | [`ReMeLongTermMemory::record`] | error propagates (Java does not swallow here) |
//!
//! **Decision (tracker §4.2, 2026-10-31):** the un-suffixed `record` / `retrieve`
//! methods are the *default* observable API and stay strict, so failures are never
//! lost silently; callers that wire memory into an agent turn (the Java
//! `LongTermMemoryHook` equivalent — a memory outage must not abort the turn) call
//! the matching `*_lossy` method. The mapping is 1:1 with the Java operators above,
//! including the log line, so porting a Java call site is mechanical: `retrieve` →
//! `retrieve_lossy`, `record` → `record_lossy` wherever the Java code ended with
//! `onErrorReturn("")` / `onErrorComplete()`.
//!
//! ## Resilience and observability
//!
//! [`RetryPolicy`] / [`RetryTransport`] add optional exponential backoff at the
//! transport seam; [`RetryPolicy::default`] is a single attempt, i.e. exactly Java's
//! behaviour (the Java clients never retry). [`MemMetrics`] /
//! [`InstrumentedTransport`] add per-attempt counters and a `mem.http.post` span; the
//! production constructors (`Mem0Client::with_api_type`, `BailianMemoryClient::with_timeout`,
//! `ReMeClient::with_timeout`) always install the instrumented transport and expose
//! their counters through `metrics()`. Per-request (Java `readTimeout`) and connect
//! (30s) timeouts stay configurable through each client's `with_timeout`
//! / `with_api_type` constructor.
//!
//! **`add` is a write, so retries can duplicate memories** (tracker MEM-05):
//! [`HttpTransport::post_json`] carries no idempotency hint, so [`RetryTransport`]
//! re-sends every POST verbatim — Mem0 `add` re-extracts memories, ReMe
//! `/summary_personal_memory` re-summarises, Bailian `add` appends a record. The
//! default [`RetryPolicy::none`] (one attempt) is Java's behaviour and therefore
//! safe; a policy with `max_attempts > 1` is the caller accepting that a retry may
//! land after a request that already succeeded server-side, and [`RetryTransport`]
//! warns once at construction. For retry-reads-only, build two clients over the same
//! base URL: one with the policy for `search`, one with [`RetryPolicy::none`] for
//! `add` (both through the client's `with_transport`).
//!
//! ## Offline by default
//!
//! All clients depend on the [`HttpTransport`] seam, so assembly can inject
//! [`http::ReqwestTransport`] in production or an in-memory mock in tests. The unit
//! test suite performs no network I/O; `tests/live_endpoints.rs` only runs when the
//! corresponding `*_LIVE_*` environment variables are set (see the tracker).

pub mod bailian;
pub mod error;
pub mod http;
pub mod mem0;
pub mod middleware;
pub mod reme;
pub mod retry;
pub mod telemetry;
pub use middleware::{
    long_term_memory_message, wrap_long_term_memory, LongTermMemoryBackend,
    MemLongTermMemoryMiddleware, LONG_TERM_MEMORY_MESSAGE_NAME, LONG_TERM_MEMORY_PREFIX,
    LONG_TERM_MEMORY_SUFFIX,
};

pub use bailian::{
    BailianAddRequest, BailianAddResponse, BailianLongTermMemory, BailianLongTermMemoryConfig,
    BailianMemoryClient, BailianMemoryNode, BailianMessage, BailianSearchRequest,
    BailianSearchResponse, ADD_MEMORY_ENDPOINT,
};
pub use error::{MemError, Result, UNSPECIFIED_OPERATION, UNSPECIFIED_VENDOR};
pub use http::{HttpTransport, ReqwestTransport, DEFAULT_CONNECT_TIMEOUT};
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
pub use retry::{RetryPolicy, RetryTransport};
pub use telemetry::{InstrumentedTransport, MemMetrics, MemMetricsSnapshot};
