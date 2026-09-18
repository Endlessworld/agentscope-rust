//! PostgreSQL-backed [`DistributedStore`] composer.
//!
//! Mirrors the Java `PostgresDistributedStore`: a single factory that wires the
//! PostgreSQL agent-state store, KV store, snapshot spec, and sandbox execution guard
//! together.
//!
//! **Harness note:** the harness `DistributedStore` currently only carries
//! `agent_state_store` + `isolation_scope`. The additional components (base store,
//! snapshot spec, guard) are exposed directly on this composer for callers to place
//! where needed, mirroring the Java aggregation without changing harness types. See the
//! module gap tracker §4.2.

use std::sync::Arc;

use agentscope_harness::agent::distributed_store::DistributedStore;

use crate::client::PostgresClient;
use crate::error::{PgResult, PostgresError};
use crate::guard::PostgresSandboxExecutionGuard;
use crate::snapshot::{PostgresRemoteSnapshotClient, PostgresSnapshotSpec};
use crate::state::PostgresAgentStateStore;
use crate::store::PostgresStore;

/// PostgreSQL-backed distributed store composer.
#[derive(Clone)]
pub struct PostgresDistributedStore {
    client: Arc<dyn PostgresClient>,
}

impl std::fmt::Debug for PostgresDistributedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresDistributedStore").finish_non_exhaustive()
    }
}

impl PostgresDistributedStore {
    /// Creates a composer over a SQL client.
    pub fn new(client: Arc<dyn PostgresClient>) -> Self {
        Self { client }
    }

    /// Creates a composer (factory alias, mirrors Java `PostgresDistributedStore.create`).
    pub fn create(client: Arc<dyn PostgresClient>) -> Self {
        Self::new(client)
    }

    /// The shared SQL client.
    pub fn client(&self) -> &Arc<dyn PostgresClient> {
        &self.client
    }

    /// Builds the agent-state store with schema/table auto-creation.
    pub fn agent_state_store(&self) -> PgResult<PostgresAgentStateStore> {
        PostgresAgentStateStore::create_if_not_exist(Arc::clone(&self.client))
    }

    /// Builds the KV [`BaseStore`](agentscope_harness::filesystem::BaseStore) with
    /// schema initialization enabled.
    pub fn base_store(&self) -> PgResult<PostgresStore> {
        PostgresStore::with_schema_init(Arc::clone(&self.client))
    }

    /// Builds the snapshot client (table auto-created).
    pub fn snapshot_client(&self) -> PgResult<PostgresRemoteSnapshotClient> {
        PostgresRemoteSnapshotClient::new(Arc::clone(&self.client), true)
            .map_err(|e| PostgresError::Schema(e.to_string()))
    }

    /// Builds the snapshot spec (table auto-created).
    pub fn snapshot_spec(&self) -> PgResult<PostgresSnapshotSpec> {
        PostgresSnapshotSpec::new(Arc::clone(&self.client))
            .map_err(|e| PostgresError::Schema(e.to_string()))
    }

    /// Builds the sandbox execution guard with default settings.
    pub fn sandbox_execution_guard(&self) -> PostgresSandboxExecutionGuard {
        PostgresSandboxExecutionGuard::new(Arc::clone(&self.client))
    }

    /// Builds a harness [`DistributedStore`] carrying the agent-state store.
    pub fn distributed_store(&self) -> PgResult<DistributedStore> {
        let state: Arc<dyn agentscope_core::state::AgentStateStore> =
            Arc::new(self.agent_state_store()?);
        Ok(DistributedStore::builder()
            .agent_state_store(state)
            .build())
    }
}
