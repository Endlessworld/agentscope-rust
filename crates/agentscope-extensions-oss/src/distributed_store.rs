//! Alibaba Cloud OSS backed [`DistributedStore`] composition, mirroring
//! `io.agentscope.extensions.oss.OssDistributedStore`.
//!
//! # Cross-module note
//!
//! The harness `DistributedStore` carries `agent_state_store` + `isolation_scope` plus the
//! composite slots `base_store`, `sandbox_snapshot_spec` and `sandbox_execution_guard`
//! (EXTENSIONS_GAP_TRACKER §2.2, resolved 2026-10-28). `OssDistributedStore` builds all
//! four OSS components and projects them into the harness bundle via
//! [`OssDistributedStore::distributed_store`].

use std::sync::Arc;

use agentscope_harness::agent::distributed_store::DistributedStore;
use crate::base_store::OssStore;
use crate::client::{normalize_prefix, OssClient};
use crate::error::{OssError, OssResult};
use crate::guard::OssSandboxExecutionGuard;
use crate::snapshot::OssSnapshotSpec;
use crate::state_store::OssAgentStateStore;

/// Default top-level key prefix used by the composed distributed store.
pub const DEFAULT_KEY_PREFIX: &str = "agentscope/";

/// OSS-backed distributed-store composition.
///
/// Holds a shared OSS client and lazily composes the agent-state store, base
/// store, snapshot spec and execution guard, each under its own sub-prefix
/// (`state/`, `store/`, `snapshot/`).
#[derive(Clone)]
pub struct OssDistributedStore {
    client: Arc<dyn OssClient>,
    bucket_name: String,
    key_prefix: String,
    guard: OssSandboxExecutionGuard,
}

impl std::fmt::Debug for OssDistributedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OssDistributedStore")
            .field("bucket_name", &self.bucket_name)
            .field("key_prefix", &self.key_prefix)
            .finish_non_exhaustive()
    }
}

impl OssDistributedStore {
    /// Creates an OSS distributed store.
    ///
    /// `key_prefix` defaults to `"agentscope/"` when `None`/blank.
    pub fn create(
        client: Arc<dyn OssClient>,
        bucket_name: impl Into<String>,
        key_prefix: Option<&str>,
    ) -> OssResult<Self> {
        let bucket_name = bucket_name.into();
        if bucket_name.trim().is_empty() {
            return Err(OssError::BlankBucket);
        }
        Ok(Self {
            client,
            bucket_name,
            key_prefix: normalize_prefix(key_prefix, DEFAULT_KEY_PREFIX),
            guard: OssSandboxExecutionGuard::new(),
        })
    }

    /// Returns the configured bucket name.
    pub fn bucket_name(&self) -> &str {
        &self.bucket_name
    }

    /// Returns the normalized top-level key prefix.
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    /// Builds the OSS-backed [`OssAgentStateStore`] (prefix `{keyPrefix}state/`).
    pub fn oss_agent_state_store(&self) -> OssResult<OssAgentStateStore> {
        OssAgentStateStore::builder()
            .client(self.client.clone())
            .bucket_name(self.bucket_name.clone())
            .key_prefix(format!("{}state/", self.key_prefix))
            .build()
    }

    /// Builds the OSS-backed [`OssStore`] (prefix `{keyPrefix}store/`).
    pub fn oss_base_store(&self) -> OssResult<OssStore> {
        OssStore::builder()
            .client(self.client.clone())
            .bucket_name(self.bucket_name.clone())
            .key_prefix(format!("{}store/", self.key_prefix))
            .build()
    }

    /// Builds the OSS-backed [`OssSnapshotSpec`] (prefix `{keyPrefix}snapshot/`).
    pub fn oss_snapshot_spec(&self) -> OssResult<OssSnapshotSpec> {
        OssSnapshotSpec::new(
            self.client.clone(),
            self.bucket_name.clone(),
            Some(&format!("{}snapshot/", self.key_prefix)),
        )
    }

    /// Returns the sandbox execution guard (harness guard, shared handle).
    ///
    /// The guard is **process-local**: `SandboxGuard::is_distributed()` returns `false`
    /// because OSS offers no server-side lock primitive, so the guard only serialises
    /// inside one process.
    pub fn sandbox_execution_guard(&self) -> OssSandboxExecutionGuard {
        self.guard.clone()
    }

    /// Composes the harness [`DistributedStore`] with every OSS component.
    ///
    /// The agent-state store, KV base store, snapshot spec and execution guard are all
    /// projected (tracker §4.2, resolved 2026-10-28).
    ///
    /// The projected `sandbox_execution_guard` slot holds [`OssSandboxExecutionGuard`],
    /// whose `is_distributed()` is `false` (see [`Self::sandbox_execution_guard`]).
    pub fn distributed_store(&self) -> OssResult<DistributedStore> {
        let ns = self.oss_agent_state_store()?;
        let base = self.oss_base_store()?;
        let snapshots = self.oss_snapshot_spec()?;
        Ok(DistributedStore::builder()
            .agent_state_store(Arc::new(ns))
            .base_store(Arc::new(base))
            .sandbox_snapshot_spec(Arc::new(snapshots))
            .sandbox_execution_guard(Arc::new(self.guard.clone()))
            .build())
    }
}
