//! Alibaba Cloud OSS backed [`DistributedStore`] composition, mirroring
//! `io.agentscope.extensions.oss.OssDistributedStore`.
//!
//! # Cross-module note
//!
//! The harness `DistributedStore` currently only carries `agent_state_store` +
//! `isolation_scope` (see EXTENSIONS_GAP_TRACKER §2.2), while the Java version
//! additionally exposes `base_store`, `sandbox_snapshot_spec` and
//! `sandbox_execution_guard`. Until the harness struct is extended (tracker
//! §4.2), the OSS crate still constructs and owns all of those components, and
//! exposes them alongside a harness [`DistributedStore`] via [`OssDistributedStore`]
//! so callers can wire the extra pieces onto `HarnessAgent` manually.

use std::sync::Arc;

use agentscope_harness::agent::distributed_store::DistributedStore;
use agentscope_harness::filesystem::remote::store::BaseStore;

use crate::base_store::{OssStore, DEFAULT_KEY_PREFIX as STORE_DEFAULT_PREFIX};
use crate::client::{normalize_prefix, OssClient};
use crate::error::{OssError, OssResult};
use crate::guard::OssSandboxExecutionGuard;
use crate::snapshot::OssSnapshotSpec;
use crate::state_store::{OssAgentStateStore, DEFAULT_KEY_PREFIX as STATE_DEFAULT_PREFIX};

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
    pub fn sandbox_execution_guard(&self) -> OssSandboxExecutionGuard {
        self.guard.clone()
    }

    /// Composes the harness [`DistributedStore`] with the OSS agent-state store.
    pub fn distributed_store(&self) -> OssResult<DistributedStore> {
        let ns = self.oss_agent_state_store()?;
        Ok(DistributedStore::builder()
            .agent_state_store(Arc::new(ns))
            .build())
    }
}

// The default prefixes are only referenced for documentation parity with Java;
// keep them available via the sub-modules and silence dead-code when unused.
#[allow(dead_code)]
const _DEFAULT_PREFIXES: (&str, &str) = (STATE_DEFAULT_PREFIX, STORE_DEFAULT_PREFIX);

/// Blanket helper: box an [`OssStore`] as an `Arc<dyn BaseStore>`.
pub fn as_arc_base_store(store: OssStore) -> Arc<dyn BaseStore> {
    Arc::new(store)
}
