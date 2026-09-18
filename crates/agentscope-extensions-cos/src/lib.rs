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
//! written against the in-crate async [`CosObjectStore`] trait. A real SDK adapter
//! (bucket/region/credentials + HTTP signing) is the remaining work item — see
//! `docs/extensions/COS__GAP_TRACKER.md` §4.2. Tests and local development use
//! [`InMemoryCosObjectStore`].
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
pub mod layout;
pub mod object_store;
pub mod snapshot;

pub use agent_state_store::{cos_agent_state_store, CosAgentStateStore, CosAgentStateStoreBuilder};
pub use base_store::{CosStore, CosStoreBuilder};
pub use distributed_store::{cos_distributed_store, CosDistributedStore};
pub use error::{CosError, Result};
pub use guard::CosSandboxExecutionGuard;
pub use object_store::{CosObjectStore, InMemoryCosObjectStore, SharedObjectStore};
pub use snapshot::{cos_remote_snapshot_client, CosRemoteSnapshotClient, CosSnapshotSpec};

/// Crate version, useful for diagnostics.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
