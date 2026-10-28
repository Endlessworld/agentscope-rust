//! Tencent Cloud COS backed distributed store composition, mirroring
//! `io.agentscope.extensions.cos.CosDistributedStore`.
//!
//! **§2.1 constraint honoured**: the harness `DistributedStore` carries
//! `agent_state_store` + `isolation_scope` plus the composite slots `base_store`,
//! `sandbox_snapshot_spec` and `sandbox_execution_guard` (resolved 2026-10-28). This crate
//! provides a COS-specific composition type ([`CosDistributedStore`]) that builds all the
//! COS components and projects them onto the harness `DistributedStore` via
//! [`CosDistributedStore::to_harness_distributed_store`].

use std::sync::Arc;

use agentscope_core::state::AgentStateStore;
use agentscope_harness::agent::sandbox::snapshot::SandboxSnapshotSpec;
use agentscope_harness::agent::DistributedStore as HarnessDistributedStore;
use agentscope_harness::filesystem::remote::store::BaseStore;

use crate::agent_state_store::CosAgentStateStore;
use crate::base_store::CosStore;
use crate::guard::CosSandboxExecutionGuard;
use crate::layout::DEFAULT_DISTRIBUTED_PREFIX;
use crate::object_store::SharedObjectStore;
use crate::snapshot::CosSnapshotSpec;
use crate::{CosError, Result};

/// COS-backed bundle of the distributed storage components.
///
/// Mirrors Java's `CosDistributedStore.create(cosClient, bucket, prefix)`: given a
/// shared object store and a top-level prefix, it derives the sub-prefixes
/// `{prefix}state/`, `{prefix}store/`, `{prefix}snapshot/` and wires up the four
/// sub-components.
#[derive(Clone)]
pub struct CosDistributedStore {
    object_store: SharedObjectStore,
    key_prefix: String,
    agent_state_store: Arc<CosAgentStateStore>,
    base_store: Arc<CosStore>,
    snapshot_spec: Arc<CosSnapshotSpec>,
    execution_guard: CosSandboxExecutionGuard,
}

impl std::fmt::Debug for CosDistributedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CosDistributedStore")
            .field("key_prefix", &self.key_prefix)
            .finish_non_exhaustive()
    }
}

impl CosDistributedStore {
    /// Creates a COS distributed store from a shared object store.
    ///
    /// `key_prefix` defaults to `"agentscope/"` when `None`/blank.
    pub fn create(object_store: SharedObjectStore, key_prefix: Option<&str>) -> Result<Self> {
        let prefix = match key_prefix {
            Some(p) if !p.trim().is_empty() => {
                crate::layout::normalize_prefix(Some(p), DEFAULT_DISTRIBUTED_PREFIX)
            }
            _ => DEFAULT_DISTRIBUTED_PREFIX.to_string(),
        };

        let agent_state_store = Arc::new(
            CosAgentStateStore::builder()
                .object_store(object_store.clone())
                .key_prefix(format!("{prefix}state/"))
                .build()?,
        );
        let base_store = Arc::new(
            CosStore::builder()
                .object_store(object_store.clone())
                .key_prefix(format!("{prefix}store/"))
                .build()?,
        );
        let snapshot_spec = Arc::new(CosSnapshotSpec::new(
            object_store.clone(),
            Some(&format!("{prefix}snapshot/")),
        ));
        let execution_guard = CosSandboxExecutionGuard::new(object_store.clone());

        Ok(Self {
            object_store,
            key_prefix: prefix,
            agent_state_store,
            base_store,
            snapshot_spec,
            execution_guard,
        })
    }

    /// The top-level key prefix in effect.
    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    /// The shared object store backing all sub-components.
    pub fn object_store(&self) -> &SharedObjectStore {
        &self.object_store
    }

    /// The COS-backed [`AgentStateStore`].
    pub fn agent_state_store(&self) -> Arc<CosAgentStateStore> {
        self.agent_state_store.clone()
    }

    /// The COS-backed [`BaseStore`].
    pub fn base_store(&self) -> Arc<CosStore> {
        self.base_store.clone()
    }

    /// The COS-backed snapshot spec.
    pub fn sandbox_snapshot_spec(&self) -> Arc<CosSnapshotSpec> {
        self.snapshot_spec.clone()
    }

    /// The COS-backed sandbox execution guard.
    ///
    /// Note the guard is **process-local** (`SandboxGuard::is_distributed() == false`): COS
    /// exposes no server-side lock, so this delegates to the harness in-process guard.
    pub fn sandbox_execution_guard(&self) -> CosSandboxExecutionGuard {
        self.execution_guard.clone()
    }

    /// Projects onto the harness [`HarnessDistributedStore`].
    ///
    /// Every COS component is carried over: the agent-state store, the KV base store, the
    /// snapshot spec, and the execution guard (through the harness `SandboxGuard` trait).
    /// The default isolation scope is used.
    ///
    /// The projected `sandbox_execution_guard` slot is **not** a distributed lock: hosts
    /// must check `SandboxGuard::is_distributed()` (it returns `false` here) before
    /// assuming cross-process sandbox serialisation.
    pub fn to_harness_distributed_store(&self) -> HarnessDistributedStore {
        let store: Arc<dyn AgentStateStore> = self.agent_state_store.clone();
        HarnessDistributedStore::builder()
            .agent_state_store(store)
            .base_store(self.base_store.clone())
            .sandbox_snapshot_spec(self.snapshot_spec.clone())
            .sandbox_execution_guard(Arc::new(self.execution_guard.clone()))
            .build()
    }

    /// Returns the agent state store as a trait object (`Arc<dyn AgentStateStore>`),
    /// ready for `HarnessAgent::builder().state_store(...)`.
    pub fn shared_agent_state_store(&self) -> Arc<dyn AgentStateStore> {
        self.agent_state_store.clone()
    }

    /// Returns the base store as a trait object (`Arc<dyn BaseStore>`).
    pub fn shared_base_store(&self) -> Arc<dyn BaseStore> {
        self.base_store.clone()
    }

    /// Returns the snapshot spec as a trait object (`Arc<dyn SandboxSnapshotSpec>`).
    pub fn shared_snapshot_spec(&self) -> Arc<dyn SandboxSnapshotSpec> {
        self.snapshot_spec.clone()
    }
}

/// Free-function constructor mirroring the Java static factory.
pub fn create(
    object_store: SharedObjectStore,
    key_prefix: Option<&str>,
) -> Result<CosDistributedStore> {
    CosDistributedStore::create(object_store, key_prefix)
}

/// Convenience constructor that validates the prefix, returning [`CosError`].
pub fn cos_distributed_store(
    object_store: SharedObjectStore,
    key_prefix: Option<&str>,
) -> Result<CosDistributedStore> {
    if let Some(p) = key_prefix {
        if p.trim().is_empty() {
            return Err(CosError::Config("keyPrefix must not be blank".into()));
        }
    }
    CosDistributedStore::create(object_store, key_prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_store::InMemoryCosObjectStore;
    use agentscope_harness::agent::sandbox::SandboxIsolationKey;

    fn distributed() -> CosDistributedStore {
        CosDistributedStore::create(InMemoryCosObjectStore::new().shared(), Some("agentscope/"))
            .unwrap()
    }

    #[test]
    fn derives_sub_prefixes() {
        let d = distributed();
        assert_eq!(d.key_prefix(), "agentscope/");
        assert_eq!(d.agent_state_store().key_prefix(), "agentscope/state/");
        assert_eq!(d.base_store().key_prefix(), "agentscope/store/");
        assert_eq!(
            d.sandbox_snapshot_spec().key_prefix(),
            "agentscope/snapshot/"
        );
    }

    #[test]
    fn default_prefix_applied() {
        let d = CosDistributedStore::create(InMemoryCosObjectStore::new().shared(), None).unwrap();
        assert_eq!(d.key_prefix(), DEFAULT_DISTRIBUTED_PREFIX);
    }

    #[tokio::test]
    async fn components_share_the_same_object_store() {
        let d = distributed();
        d.shared_agent_state_store()
            .save(Some("u"), "s", "k", serde_json::json!(1))
            .await;
        assert_eq!(
            d.shared_agent_state_store()
                .get(Some("u"), "s", "k")
                .await
                .unwrap(),
            serde_json::json!(1)
        );
        // base store + snapshot client share the same backing store
        assert!(d.object_store().len().await >= 1);
    }

    #[test]
    fn harness_projection_carries_agent_state_store() {
        let d = distributed();
        let hs = d.to_harness_distributed_store();
        assert!(hs.agent_state_store.is_some());
    }

    /// Tracker §2.2 resolved 2026-10-28: base store, snapshot spec and guard are projected,
    /// and the guard is usable through the object-safe `SandboxGuard` trait.
    #[tokio::test]
    async fn harness_projection_fills_every_composite_slot() {
        let d = distributed();
        let hs = d.to_harness_distributed_store();

        let base: &Arc<dyn BaseStore> = hs.base_store.as_ref().expect("base_store projected");
        let mut value = serde_json::Map::new();
        value.insert("projected".to_string(), serde_json::json!(true));
        base.put(&["ns".to_string()], "k", value);
        assert!(base.get(&["ns".to_string()], "k").is_some());

        let spec = hs
            .sandbox_snapshot_spec
            .as_ref()
            .expect("sandbox_snapshot_spec projected");
        assert_eq!(spec.build("projected").expect("build").id(), "projected");

        let guard = hs
            .sandbox_execution_guard
            .as_ref()
            .expect("sandbox_execution_guard projected");
        let key = SandboxIsolationKey::new("user", "u1");
        let lease = guard.try_enter(key.clone()).await;
        assert!(!lease.is_noop());
        assert!(guard.is_held(&key));
        guard.release(&key).await;
        assert!(!guard.is_held(&key));
    }

    #[test]
    fn guard_composes_with_distributed() {
        let d = distributed();
        let guard = d.sandbox_execution_guard();
        let key = SandboxIsolationKey::new("user", "u1");
        let _lease = guard.try_enter(key.clone());
        assert!(guard.is_held(&key));
    }
}
