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
//! Object access goes through the [`OssClient`] trait. The in-memory
//! [`InMemoryOssClient`] is provided for tests/local development.
//!
//! Enabling the **`s3`** feature (off by default) compiles [`S3OssClient`], a
//! generic S3-compatible backend built on [`rust-s3`](https://crates.io/crates/rust-s3).
//! It works with AWS S3, MinIO, Ceph, and S3-compatible endpoints such as
//! Alibaba Cloud OSS / Cloudflare R2. Configure bucket, region (default
//! `us-east-1`), optional custom `endpoint` (path-style addressing is enabled
//! automatically for custom endpoints), access key / secret key (falling back
//! to `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`), and an optional session
//! token.
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
//! > **Blocking note:** [`OssClient`] is synchronous and [`S3OssClient`] drives
//! > its requests on a dedicated Tokio runtime via `block_on`. Do **not** call
//! > its methods directly from a Tokio worker thread; wrap them in
//! > `tokio::task::spawn_blocking` (or `block_in_place`) instead.

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
pub use client::{InMemoryOssClient, OssClient, OssListPage, OssObjectSummary};
#[cfg(feature = "s3")]
pub use s3::{S3OssClient, S3OssClientBuilder};
pub use distributed_store::OssDistributedStore;
pub use error::{OssError, OssResult};
pub use guard::OssSandboxExecutionGuard;
pub use snapshot::{OssRemoteSnapshotClient, OssSnapshotSpec};
pub use state_store::{OssAgentStateStore, OssAgentStateStoreBuilder};

// Re-export the harness guard type for callers that need the raw handle.
pub use agentscope_harness::agent::sandbox::SandboxExecutionGuard;
