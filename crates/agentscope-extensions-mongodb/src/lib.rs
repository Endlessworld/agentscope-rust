//! # agentscope-extensions-mongodb
//!
//! MongoDB-backed implementations of the harness/core storage traits, mirroring the
//! Java module `agentscope-extensions-mongodb`.
//!
//! This crate **reuses the existing harness traits** rather than inventing parallel
//! abstractions (see `docs/extensions/EXTENSIONS_GAP_TRACKER.md` §2.1):
//!
//! | Java component | Rust output | Implements |
//! |---|---|---|
//! | `MongoDistributedStore` | [`MongodbDistributedStore`] | composes the four below |
//! | `MongoAgentStateStore` | [`MongodbAgentStateStore`] | `agentscope_core::state::AgentStateStore` |
//! | `MongoBaseStore` | [`MongodbStore`] | `agentscope_harness::filesystem::remote::store::BaseStore` |
//! | `MongoSnapshotSpec` | [`MongodbSnapshotSpec`] | `agentscope_harness::agent::sandbox::snapshot::SandboxSnapshotSpec` |
//! | `MongoSandboxExecutionGuard` | [`MongodbSandboxExecutionGuard`] / [`MongodbGuardClient`] | `agentscope_harness::agent::sandbox::SandboxGuard` (the guard *trait*) |
//!
//! ## Driver-agnostic design
//!
//! The Java module depends directly on the MongoDB Java driver. This crate models the
//! small slice of the driver API it needs behind the [`client::MongoClient`] trait, so
//! the store logic is transport-agnostic. Two implementations ship:
//!
//! * [`client::InMemoryMongoClient`] — a faithful in-process backend used by the
//!   contract tests (dependency-free, fully offline);
//! * [`MongodbClient`] — the **real driver** backend (`mongodb` 3.x), used in production
//!   and exercised by the env-gated live tests in `tests/mongodb_live.rs`
//!   (`AGENTSCOPE_TEST_MONGODB_URL`).
//!
//! The guard is exposed twice on purpose: [`MongodbSandboxExecutionGuard`] is the
//! MongoDB-specific lock (inherent `try_enter` / `release_async`), while
//! [`MongodbGuardClient`] adapts it to the object-safe
//! [`agentscope_harness::agent::sandbox::SandboxGuard`] trait so it can be projected into
//! `DistributedStore::sandbox_execution_guard` and driven by `SandboxManager`.
//!
//! ## CAS / version semantics
//!
//! Versioning follows the harness definitions: `StoreItem.version` starts at 1 and
//! increments on every successful write; `put_if_version(expected = 0)` means
//! "create only if absent". The state store additionally exposes
//! [`MongodbAgentStateStore::save_if_version`] / [`MongodbAgentStateStore::get_version`]
//! with the same semantics as the Java store.
//!
//! ## Quick start
//!
//! ```no_run
//! use std::sync::Arc;
//! use agentscope_extensions_mongodb::MongodbClient;
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! // Real driver (pings the deployment so a bad URI fails immediately).
//! let client = Arc::new(MongodbClient::connect("mongodb://127.0.0.1:27017").await?);
//! let store = agentscope_extensions_mongodb::MongodbDistributedStore::new(client, Some("agentscope"));
//! store.ensure_indexes().await?;
//! # Ok(()) }
//! ```
//!
//! Offline (no server needed) — the same store logic over the in-process backend:
//!
//! ```
//! use std::sync::Arc;
//! use agentscope_extensions_mongodb::{MongodbDistributedStore, client::InMemoryMongoClient};
//!
//! # async fn demo() -> agentscope_core::error::Result<()> {
//! let store = MongodbDistributedStore::new(Arc::new(InMemoryMongoClient::new()), Some("agentscope"));
//! store.ensure_indexes().await?;
//! # Ok(()) }
//! ```

pub mod client;
pub mod client_mongodb;
pub mod constants;
pub mod distributed_store;
pub mod index_utils;
pub mod key_escaper;
pub mod list_hash;
pub mod sandbox_guard;
pub mod snapshot;
pub mod state_store;
pub mod store;

pub use client_mongodb::MongodbClient;
pub use constants::{
    BASE_STORE_COLLECTION, DEFAULT_DATABASE, SANDBOX_LOCKS_COLLECTION, SESSIONS_COLLECTION,
    SNAPSHOTS_COLLECTION,
};
pub use distributed_store::MongodbDistributedStore;
pub use index_utils::{create_index_with_migration, IndexSpec};
pub use sandbox_guard::{MongodbGuardClient, MongodbSandboxExecutionGuard};
pub use snapshot::{MongodbRemoteSnapshotClient, MongodbSnapshotSpec};
pub use state_store::{MongodbAgentStateStore, UNVERSIONED};
pub use store::MongodbStore;
