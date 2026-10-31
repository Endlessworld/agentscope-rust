//! agentscope-extensions-oss — S3-compatible object storage for AgentScope.
//!
//! Mirrors the Java module `io.agentscope.extensions.oss`. Implements the
//! harness/core storage traits instead of inventing parallel abstractions
//! (see `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §2.1):
//!
//! | Java class | Rust type | Implements |
//! |---|---|---|
//! | `OssAgentStateStore` | [`OssAgentStateStore`] | [`agentscope_core::state::AgentStateStore`] |
//! | `OssBaseStore` | [`OssStore`] | [`agentscope_harness::filesystem::remote::store::BaseStore`] |
//! | `OssRemoteSnapshotClient` / `OssSnapshotSpec` | [`OssRemoteSnapshotClient`] / [`OssSnapshotSpec`] | [`agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient`] / `SandboxSnapshotSpec` |
//! | `OssDistributedStore` | [`OssDistributedStore`] | composes the above |
//! | (harness guard) | [`OssSandboxExecutionGuard`] | wrapper over harness `SandboxExecutionGuard` |
//!
//! # Backend abstraction
//!
//! Object access goes through the [`OssClient`] trait; a backend is **never
//! selected implicitly**:
//!
//! * the default build (no features) ships only [`InMemoryOssClient`] — objects
//!   live in process memory, so it is intended for tests, local development and
//!   as a reference implementation of the trait;
//! * the optional **`s3`** feature (off by default) additionally *compiles*
//!   `S3OssClient`, a generic S3-compatible backend built on
//!   [`rust-s3`](https://crates.io/crates/rust-s3). Turning the feature on does
//!   **not** create a backend by itself and does **not** touch any bucket: you
//!   must construct the client explicitly, and that requires configuration —
//!   bucket (mandatory), region (default `us-east-1`), and credentials (access
//!   key / secret key, falling back to `AWS_ACCESS_KEY_ID` /
//!   `AWS_SECRET_ACCESS_KEY`, plus an optional session token). A custom
//!   `endpoint` is needed for MinIO / Ceph / Cloudflare R2 / OSS-S3 (path-style
//!   addressing is enabled automatically for custom endpoints). Building
//!   without credentials fails with `OssError::MissingCredentials`.
//!
//! Pagination is part of the contract: [`OssClient::list_page`] returns at most
//! one backend page plus a continuation token, and [`OssClient::list_all`]
//! follows the tokens. [`InMemoryOssClient::with_page_size`] emulates a page cap
//! so multi-page paths can be tested offline.
//!
//! # Compare-and-set
//!
//! A backend that implements [`OssClient::get_with_etag`] and
//! [`OssClient::put_conditional`] (both in-memory and S3 clients do) can serve as an
//! optimistic-concurrency anchor: [`OssStore`] then declares
//! `supports_atomic_session_storage()` and [`OssAgentStateStore`] declares
//! `supports_versioning()`, so both are single-shot backend-evaluated preconditions
//! instead of a read-check-write race. A client without them keeps the Java behaviour
//! and is refused by the harness session journal.
//!
//! ```no_run
//! # #[cfg(feature = "s3")]
//! # fn main() {
//! use std::sync::Arc;
//! use agentscope_extensions_oss::{OssAgentStateStore, S3OssClient};
//!
//! let client = Arc::new(
//!     S3OssClient::builder()
//!         .bucket("my-bucket")
//!         .region("us-east-1")
//!         // MinIO / Ceph / OSS-S3: set a custom endpoint.
//!         .endpoint("http://127.0.0.1:9000")
//!         .access_key("access-key")
//!         .secret_key("secret-key")
//!         .build()
//!         .unwrap(),
//! );
//!
//! let store = OssAgentStateStore::builder()
//!     .client(client)
//!     .bucket_name("my-bucket")
//!     .key_prefix("agentscope/state/")
//!     .build()
//!     .unwrap();
//! # }
//! # #[cfg(not(feature = "s3"))]
//! # fn main() {}
//! ```
//!
//! > **Blocking note:** [`OssClient`] is synchronous. `S3OssClient` does **not**
//! > `block_on` on the caller's thread: every request is shipped to a dedicated
//! > worker thread that owns the Tokio runtime, so the stores can be used from
//! > async contexts (Tokio worker threads, current-thread runtimes) as well as
//! > from plain threads without a nested-runtime panic. Note the caller thread
//! > still *blocks* for the duration of a request (that is what a synchronous
//! > trait means), so very high-throughput async callers should still
//! > `tokio::task::spawn_blocking` around batch operations.

pub mod base_store;
pub mod client;
pub mod distributed_store;
pub mod error;
pub mod guard;
#[cfg(feature = "s3")]
pub mod s3;
pub mod snapshot;
pub mod state_store;

pub use base_store::{OssStore, OssStoreBuilder};
pub use client::{
    etag_version_token, InMemoryOssClient, OssClient, OssListPage, OssObjectSummary,
    OssObjectVersion, PutCondition, PutOutcome,
};
pub use distributed_store::OssDistributedStore;
pub use error::{OssError, OssResult};
pub use guard::OssSandboxExecutionGuard;
#[cfg(feature = "s3")]
pub use s3::{S3OssClient, S3OssClientBuilder};
pub use snapshot::{OssRemoteSnapshotClient, OssSnapshotSpec};
pub use state_store::{OssAgentStateStore, OssAgentStateStoreBuilder};

// Re-export the harness guard type for callers that need the raw handle.
pub use agentscope_harness::agent::sandbox::SandboxExecutionGuard;
