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

/// Environment key carrying the control-plane REST base URL.
pub const ENV_CONTROL_PLANE_HTTP: &str = "CONTROL_PLANE_HTTP";
/// Environment key carrying the trusted bootstrap token.
pub const ENV_BUILDER_INTERNAL_TOKEN: &str = "BUILDER_INTERNAL_TOKEN";
/// Environment key carrying the logical agent name.
pub const ENV_CONTROL_PLANE_AGENT_NAME: &str = "CONTROL_PLANE_AGENT_NAME";
/// Environment key carrying the tenant / Kubernetes namespace.
pub const ENV_CONTROL_PLANE_NAMESPACE: &str = "CONTROL_PLANE_NAMESPACE";

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

    /// Creates a factory from environment variables, mirroring
    /// `ControlPlaneStores.fromEnv`:
    /// `CONTROL_PLANE_HTTP`, `BUILDER_INTERNAL_TOKEN`, `CONTROL_PLANE_AGENT_NAME` (all
    /// required) and `CONTROL_PLANE_NAMESPACE` (defaults to `default`).
    ///
    /// The keys are the renamed control-plane deployment contract. Java exposes no legacy
    /// `CONTROLPLANE_*` alias, so this port deliberately does not either — reading a legacy
    /// key would silently diverge from the control plane it talks to.
    pub fn from_env() -> ControlPlaneResult<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Environment-independent core of [`from_env`](Self::from_env).
    ///
    /// `lookup` receives the keys in evaluation order, so the env contract can be asserted
    /// offline without mutating the process environment.
    pub fn from_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> ControlPlaneResult<Self> {
        let http_base = require_key(&mut lookup, ENV_CONTROL_PLANE_HTTP)?;
        let token = require_key(&mut lookup, ENV_BUILDER_INTERNAL_TOKEN)?;
        let agent_name = require_key(&mut lookup, ENV_CONTROL_PLANE_AGENT_NAME)?;
        let namespace = lookup(ENV_CONTROL_PLANE_NAMESPACE).unwrap_or_else(|| "default".into());
        Self::create(http_base, token, agent_name, namespace)
    }

    /// Creates a factory reusing connection settings from an [`ControlPlaneConfig`].
    pub fn from_config(config: &ControlPlaneConfig) -> ControlPlaneResult<Self> {
        if config.control_plane_http.trim().is_empty() {
            return Err(ControlPlaneError::Config(
                "controlPlaneHttp is required".into(),
            ));
        }
        if config.internal_token.trim().is_empty() {
            return Err(ControlPlaneError::Config(
                "internalToken is required".into(),
            ));
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

/// Reads a required environment value, reporting the key that is missing.
fn require_key(
    lookup: &mut dyn FnMut(&str) -> Option<String>,
    name: &str,
) -> ControlPlaneResult<String> {
    lookup(name).ok_or_else(|| ControlPlaneError::Config(format!("{name} is required")))
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

    /// The env contract must match `ControlPlaneStores.fromEnv` key-for-key, in order, and
    /// must not fall back to the legacy `CONTROLPLANE_*` names.
    #[test]
    fn from_lookup_reads_the_renamed_control_plane_env_keys() {
        let mut seen = Vec::new();
        let stores = ControlPlaneStores::from_lookup(|key| {
            seen.push(key.to_string());
            match key {
                "CONTROL_PLANE_HTTP" => Some("http://cp:8080".to_string()),
                "BUILDER_INTERNAL_TOKEN" => Some("tok".to_string()),
                "CONTROL_PLANE_AGENT_NAME" => Some("friday".to_string()),
                "CONTROL_PLANE_NAMESPACE" => Some("team-a".to_string()),
                _ => None,
            }
        })
        .unwrap();
        assert_eq!(
            seen,
            vec![
                ENV_CONTROL_PLANE_HTTP,
                ENV_BUILDER_INTERNAL_TOKEN,
                ENV_CONTROL_PLANE_AGENT_NAME,
                ENV_CONTROL_PLANE_NAMESPACE,
            ]
        );
        assert_eq!(stores.agent_name(), "friday");
        assert_eq!(stores.namespace(), "team-a");
        assert_eq!(stores.http().base_url(), "http://cp:8080");
    }

    /// A blank namespace falls back to `default`, exactly like Java `envOr(..., "default")`.
    #[test]
    fn from_lookup_defaults_the_namespace_and_reports_missing_keys() {
        let stores = ControlPlaneStores::from_lookup(|key| match key {
            "CONTROL_PLANE_HTTP" => Some("http://cp".to_string()),
            "BUILDER_INTERNAL_TOKEN" => Some("tok".to_string()),
            "CONTROL_PLANE_AGENT_NAME" => Some("friday".to_string()),
            _ => None,
        })
        .unwrap();
        assert_eq!(stores.namespace(), "default");

        let err = ControlPlaneStores::from_lookup(|key| match key {
            "CONTROL_PLANE_HTTP" => Some("http://cp".to_string()),
            _ => None,
        })
        .err()
        .expect("missing token must fail");
        assert!(
            err.to_string()
                .contains("BUILDER_INTERNAL_TOKEN is required"),
            "{err}"
        );
    }
}
