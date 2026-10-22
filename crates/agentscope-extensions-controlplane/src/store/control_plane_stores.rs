//! Factory for control-plane hosted store components, mirroring `ControlPlaneStores`.
//!
//! [`AgentStateStore`](agentscope_core::state::AgentStateStore) is deliberately excluded —
//! callers must supply their own via [`ControlPlaneStores::with_agent_state_store`].

use std::sync::Arc;

use agentscope_core::state::AgentStateStore;

use crate::config::ControlPlaneConfig;
use crate::error::{ControlPlaneError, ControlPlaneResult};
use crate::store::async_tool_registry::ControlPlaneAsyncToolRegistry;
use crate::store::base_store::ControlPlaneBaseStore;
use crate::store::message_bus::ControlPlaneMessageBus;
use crate::store::snapshot_spec::ControlPlaneSnapshotSpec;
use crate::transport::ControlPlaneHttpClient;

/// Factory for control-plane hosted store components (`/api/v1/dp/*`).
#[derive(Clone)]
pub struct ControlPlaneStores {
    http: Arc<ControlPlaneHttpClient>,
    agent_name: String,
    namespace: String,
    agent_state_store: Option<Arc<dyn AgentStateStore>>,
}

impl ControlPlaneStores {
    /// Creates a factory from explicit connection settings.
    pub fn create(
        http_base: impl Into<String>,
        token: impl Into<String>,
        agent_name: impl Into<String>,
        namespace: impl Into<String>,
    ) -> ControlPlaneResult<Self> {
        let agent_name = agent_name.into();
        if agent_name.trim().is_empty() {
            return Err(ControlPlaneError::Config("agentName is required".into()));
        }
        let namespace = namespace.into();
        let namespace = if namespace.trim().is_empty() {
            crate::config::DEFAULT_NAMESPACE.to_string()
        } else {
            namespace
        };
        Ok(Self {
            http: Arc::new(ControlPlaneHttpClient::new(http_base, token)?),
            agent_name,
            namespace,
            agent_state_store: None,
        })
    }

    /// Creates a factory from environment variables:
    /// `CONTROL_PLANE_HTTP`, `BUILDER_INTERNAL_TOKEN`, `CONTROL_PLANE_AGENT_NAME`,
    /// `CONTROL_PLANE_NAMESPACE`.
    pub fn from_env() -> ControlPlaneResult<Self> {
        let http_base = require_env("CONTROL_PLANE_HTTP")?;
        let token = require_env("BUILDER_INTERNAL_TOKEN")?;
        let agent_name = require_env("CONTROL_PLANE_AGENT_NAME")?;
        let namespace = std::env::var("CONTROL_PLANE_NAMESPACE").unwrap_or_else(|_| "default".into());
        Self::create(http_base, token, agent_name, namespace)
    }

    /// Creates a factory reusing connection settings from an [`ControlPlaneConfig`].
    pub fn from_config(config: &ControlPlaneConfig) -> ControlPlaneResult<Self> {
        if config.control_plane_http.trim().is_empty() {
            return Err(ControlPlaneError::Config("controlPlaneHttp is required".into()));
        }
        if config.internal_token.trim().is_empty() {
            return Err(ControlPlaneError::Config("internalToken is required".into()));
        }
        let mut s = Self::create(
            config.control_plane_http.clone(),
            config.internal_token.clone(),
            config.agent_key.clone(),
            config.namespace.clone(),
        )?;
        s.agent_state_store = None;
        Ok(s)
    }

    /// Supplies the agent-state store the caller owns.
    pub fn with_agent_state_store(mut self, store: Arc<dyn AgentStateStore>) -> Self {
        self.agent_state_store = Some(store);
        self
    }

    /// The agent-state store configured on this factory, if any.
    pub fn agent_state_store(&self) -> Option<Arc<dyn AgentStateStore>> {
        self.agent_state_store.clone()
    }

    /// Control-plane agent name.
    pub fn agent_name(&self) -> &str {
        &self.agent_name
    }

    /// Control-plane namespace.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// The shared HTTP client.
    pub fn http(&self) -> Arc<ControlPlaneHttpClient> {
        Arc::clone(&self.http)
    }

    /// Builds a control-plane hosted [`BaseStore`](agentscope_harness::filesystem::remote::store::BaseStore).
    pub fn base_store(&self) -> ControlPlaneBaseStore {
        ControlPlaneBaseStore::new(
            Arc::clone(&self.http),
            self.agent_name.clone(),
            self.namespace.clone(),
        )
    }

    /// Builds a control-plane hosted [`MessageBus`](agentscope_harness::agent::bus::MessageBus).
    pub fn message_bus(&self) -> ControlPlaneMessageBus {
        ControlPlaneMessageBus::new(
            Arc::clone(&self.http),
            self.agent_name.clone(),
            self.namespace.clone(),
        )
    }

    /// Builds a control-plane hosted async tool registry.
    pub fn async_tool_registry(&self) -> ControlPlaneAsyncToolRegistry {
        ControlPlaneAsyncToolRegistry::new(
            Arc::clone(&self.http),
            self.agent_name.clone(),
            self.namespace.clone(),
        )
    }

    /// Builds a control-plane hosted sandbox snapshot spec.
    pub fn snapshot_spec(&self) -> ControlPlaneSnapshotSpec {
        ControlPlaneSnapshotSpec::new(
            Arc::clone(&self.http),
            self.agent_name.clone(),
            self.namespace.clone(),
        )
    }
}

fn require_env(name: &str) -> ControlPlaneResult<String> {
    std::env::var(name).map_err(|_| ControlPlaneError::Config(format!("{name} is required")))
}

/// Standard tenant query string appended to every `/api/v1/dp/*` call.
pub(crate) fn tenant_query(agent_name: &str, namespace: &str) -> String {
    format!(
        "agentName={}&namespace={}",
        crate::transport::encode_query(agent_name),
        crate::transport::encode_query(namespace)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_agent_name() {
        assert!(ControlPlaneStores::create("http://cp", "tok", "", "ns").is_err());
    }

    #[test]
    fn from_config_requires_http_and_token() {
        let cfg = ControlPlaneConfig::builder("friday").build().unwrap();
        assert!(ControlPlaneStores::from_config(&cfg).is_err());
        let cfg = ControlPlaneConfig::builder("friday")
            .control_plane_http("http://cp")
            .internal_token("tok")
            .build()
            .unwrap();
        assert!(ControlPlaneStores::from_config(&cfg).is_ok());
    }
}
