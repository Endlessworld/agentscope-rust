//! Tencent Cloud COS storage extension for AgentScope Rust, mirroring
//! `io.agentscope.extensions.cos`.
//!
//! Per `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §2.1, this crate **does not**
//! invent parallel abstractions — it implements the existing core/harness traits:
//!
//! | This crate | Implements |
//! |---|---|
//! | [`CosAgentStateStore`] | `agentscope_core::state::AgentStateStore` |
//! | [`CosStore`] | `agentscope_harness::filesystem::remote::store::BaseStore` |
//! | [`CosRemoteSnapshotClient`] | `agentscope_harness::agent::sandbox::snapshot::RemoteSnapshotClient` |
//! | [`CosSnapshotSpec`] | `agentscope_harness::agent::sandbox::snapshot::SandboxSnapshotSpec` |
//! | [`CosSandboxExecutionGuard`] | wraps `agentscope_harness::agent::sandbox::SandboxExecutionGuard` |
//! | [`CosDistributedStore`] | COS composition; projects onto harness `DistributedStore` |
//!
//! ## Backend abstraction
//!
//! The Java implementation signs requests through `com.qcloud.cos.COSClient`. No
//! Tencent COS SDK is available as a vendored Rust dependency, so all components are
//! written against the in-crate async [`CosObjectStore`] trait, and
//! [`CosHttpObjectStore`] speaks the COS XML API directly (v5 signature, multipart
//! upload for large bodies, presigned URLs). Tests and local development use
//! [`InMemoryCosObjectStore`].
//!
//! Objects at or above `DEFAULT_MULTIPART_THRESHOLD` (5 MiB) are uploaded in parts;
//! see [`multipart`] for the part-size rules (ported from the Java SDK
//! `TransferManager`) and [`presign`] for presigned URLs. STS session tokens are
//! supported in the header and in presigned URLs, but **refreshing them is the
//! caller's responsibility** (rebuild via [`CosHttpObjectStore::with_credentials`]).
//!
//! ## Example
//!
//! ```no_run
//! use agentscope_extensions_cos::{CosDistributedStore, InMemoryCosObjectStore};
//!
//! let store = CosDistributedStore::create(
//!     InMemoryCosObjectStore::new().shared(),
//!     Some("agentscope/"),
//! ).unwrap();
//! let _state_store = store.shared_agent_state_store();
//! ```

pub mod agent_state_store;
pub mod base_store;
pub mod distributed_store;
pub mod error;
pub mod guard;
pub mod http_object_store;
pub mod layout;
pub mod multipart;
pub mod object_store;
pub mod presign;
pub mod signature;
pub mod snapshot;

pub use agent_state_store::{cos_agent_state_store, CosAgentStateStore, CosAgentStateStoreBuilder};
pub use base_store::{CosStore, CosStoreBuilder};
pub use distributed_store::{cos_distributed_store, CosDistributedStore};
pub use error::{CosError, Result};
pub use guard::CosSandboxExecutionGuard;
pub use http_object_store::{
    ClockFn, CosHttpConfig, CosHttpObjectStore, CosHttpObjectStoreBuilder,
    DEFAULT_SIGN_EXPIRE_SECS, MAX_DELETE_BATCH, MAX_LIST_KEYS,
};
pub use multipart::{
    MultipartPlan, DEFAULT_MULTIPART_CONCURRENCY, DEFAULT_MULTIPART_PART_SIZE,
    DEFAULT_MULTIPART_THRESHOLD, MAX_MULTIPART_PARTS, MAX_MULTIPART_PART_SIZE,
    MIN_MULTIPART_PART_SIZE,
};
pub use object_store::{
    CosObjectStore, InMemoryCosObjectStore, ObjectListPage, SharedObjectStore,
    DEFAULT_LIST_PAGE_SIZE,
};
pub use presign::MAX_PRESIGN_EXPIRE_SECS;
pub use signature::{CosCredentials, CosSignature};
pub use snapshot::{cos_remote_snapshot_client, CosRemoteSnapshotClient, CosSnapshotSpec};

/// Crate version, useful for diagnostics.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
