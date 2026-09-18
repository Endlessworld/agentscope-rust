//! agentscope-extensions-oss — Alibaba Cloud OSS backed storage for AgentScope.
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
//! [`InMemoryOssClient`] is provided for tests/local development; a concrete
//! Alibaba Cloud backend can be supplied by enabling the `aliyun` feature (see
//! crate `Cargo.toml`) or by implementing [`OssClient`] over any SDK.
//!
//! ```no_run
//! use std::sync::Arc;
//! use agentscope_extensions_oss::{InMemoryOssClient, OssAgentStateStore};
//!
//! let client = Arc::new(InMemoryOssClient::new());
//! let store = OssAgentStateStore::builder()
//!     .client(client)
//!     .bucket_name("my-agentscope-bucket")
//!     .key_prefix("agentscope/state/")
//!     .build()
//!     .unwrap();
//! ```

pub mod base_store;
pub mod client;
pub mod distributed_store;
pub mod error;
pub mod guard;
pub mod snapshot;
pub mod state_store;

pub use base_store::{OssStore, OssStoreBuilder};
pub use client::{InMemoryOssClient, OssClient, OssListPage, OssObjectSummary};
pub use distributed_store::OssDistributedStore;
pub use error::{OssError, OssResult};
pub use guard::OssSandboxExecutionGuard;
pub use snapshot::{OssRemoteSnapshotClient, OssSnapshotSpec};
pub use state_store::{OssAgentStateStore, OssAgentStateStoreBuilder};

// Re-export the harness guard type for callers that need the raw handle.
pub use agentscope_harness::agent::sandbox::SandboxExecutionGuard;
