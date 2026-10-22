//! Connection and reporting settings for an [`ControlPlaneBridge`](crate::bridge::ControlPlaneBridge),
//! mirroring `io.agentscope.extensions.controlplane.ControlPlaneConfig`.

use crate::error::{ControlPlaneError, ControlPlaneResult};

/// Default tenant / namespace applied when the caller leaves them blank.
pub const DEFAULT_TENANT: &str = "default";
/// Default namespace applied when the caller leaves it blank.
pub const DEFAULT_NAMESPACE: &str = "default";

/// Connection and reporting settings for a bridge.
///
/// Standalone BYO (recommended locally): set [`control_plane_http`](Self::control_plane_http) +
/// [`internal_token`](Self::internal_token), keep `start_grpc = false`. The bridge serves
/// `/agentscope/*` and self-registers via `POST /api/v1/agent-registrations`.
///
/// Optional ASDP gRPC: set [`control_plane`](Self::control_plane) (`host:port`) and
/// `start_grpc = true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlPlaneConfig {
    /// control plane ASDP gRPC endpoint, `host:port` (optional).
    pub control_plane: String,
    /// controlplaned REST base URL for HTTP self-register, e.g. `http://localhost:8081`.
    pub control_plane_http: String,
    /// Trusted bootstrap/workload token for first registration.
    pub internal_token: String,
    /// Agent registration credential for an existing logical identity.
    pub registration_credential: String,
    /// Stable Catalog Agent UUID when registration is performed out of band.
    pub agent_id: String,
    /// Stable external Binding UUID when registration is performed out of band.
    pub binding_id: String,
    /// Registered AgentInstance generation when out of band.
    pub generation: i64,
    /// Stable logical key registered in the control plane (required).
    pub agent_key: String,
    /// Collaboration tenant.
    pub tenant: String,
    /// Collaboration / Kubernetes namespace.
    pub namespace: String,
    /// This replica's stable key; defaults to `HOSTNAME` then the local host name.
    pub instance_key: String,
    /// Whether to persist and push the Level-2 event stream.
    pub enable_events: bool,
    /// Directory for the durable event outbox; empty uses `~/.agentscope/service-controlplane/event-journal`.
    pub event_journal_dir: String,
    /// Port for the in-process `/agentscope/*` contract server; `0` binds an ephemeral port.
    pub contract_http_port: u16,
    /// Bind address, empty for all interfaces.
    pub contract_http_host: String,
    /// URL the control plane should use to reach the contract server.
    pub public_base_url: String,
    /// Affinity hint the control plane uses when routing session commands.
    pub session_affinity: String,
    /// Whether to start the contract server.
    pub start_http: bool,
    /// Whether to open the ASDP upstream channel.
    pub start_grpc: bool,
    /// Whether to POST `/api/v1/agent-registrations`.
    pub start_http_register: bool,
}

impl ControlPlaneConfig {
    /// Starts building a config for the given logical agent key.
    pub fn builder(agent_key: impl Into<String>) -> ControlPlaneConfigBuilder {
        ControlPlaneConfigBuilder::new(agent_key)
    }

    /// Validates and normalizes, mirroring the Java canonical constructor.
    ///
    /// - `agent_key` is required.
    /// - `tenant` / `namespace` / `instance_key` fall back to defaults.
    /// - `start_grpc` requires a non-blank `control_plane`.
    pub fn validate(mut self) -> ControlPlaneResult<Self> {
        if self.agent_key.trim().is_empty() {
            return Err(ControlPlaneError::Config("agentKey is required".into()));
        }
        if self.tenant.trim().is_empty() {
            self.tenant = DEFAULT_TENANT.to_string();
        }
        if self.namespace.trim().is_empty() {
            self.namespace = DEFAULT_NAMESPACE.to_string();
        }
        if self.instance_key.trim().is_empty() {
            self.instance_key = default_instance_id();
        }
        self.control_plane_http = self.control_plane_http.trim().to_string();
        self.registration_credential = self.registration_credential.trim().to_string();
        self.agent_id = self.agent_id.trim().to_string();
        self.binding_id = self.binding_id.trim().to_string();
        self.event_journal_dir = self.event_journal_dir.trim().to_string();
        self.public_base_url = self.public_base_url.trim().to_string();
        if self.start_grpc && self.control_plane.trim().is_empty() {
            return Err(ControlPlaneError::Config(
                "controlPlane is required when gRPC is enabled".into(),
            ));
        }
        if self.start_grpc
            && !self.start_http_register
            && (self.agent_id.trim().is_empty()
                || self.binding_id.trim().is_empty()
                || self.generation <= 0)
        {
            return Err(ControlPlaneError::Config(
                "agentId, bindingId and generation are required when gRPC is enabled without HTTP register"
                    .into(),
            ));
        }
        Ok(self)
    }
}

/// Builder for [`ControlPlaneConfig`].
#[derive(Debug, Clone)]
pub struct ControlPlaneConfigBuilder {
    config: ControlPlaneConfig,
}

impl ControlPlaneConfigBuilder {
    fn new(agent_key: impl Into<String>) -> Self {
        Self {
            config: ControlPlaneConfig {
                control_plane: String::new(),
                control_plane_http: String::new(),
                internal_token: String::new(),
                registration_credential: String::new(),
                agent_id: String::new(),
                binding_id: String::new(),
                generation: 0,
                agent_key: agent_key.into(),
                tenant: DEFAULT_TENANT.to_string(),
                namespace: DEFAULT_NAMESPACE.to_string(),
                instance_key: String::new(),
                enable_events: false,
                event_journal_dir: String::new(),
                contract_http_port: 0,
                contract_http_host: String::new(),
                public_base_url: String::new(),
                session_affinity: String::new(),
                start_http: false,
                start_grpc: false,
                start_http_register: false,
            },
        }
    }

    /// control plane ASDP gRPC endpoint, `host:port`.
    pub fn control_plane(mut self, v: impl Into<String>) -> Self {
        self.config.control_plane = v.into();
        self
    }

    /// controlplaned REST base URL for HTTP self-register.
    pub fn control_plane_http(mut self, v: impl Into<String>) -> Self {
        self.config.control_plane_http = v.into();
        self
    }

    /// Internal bootstrap/workload token.
    pub fn internal_token(mut self, v: impl Into<String>) -> Self {
        self.config.internal_token = v.into();
        self
    }

    /// Agent registration credential.
    pub fn registration_credential(mut self, v: impl Into<String>) -> Self {
        self.config.registration_credential = v.into();
        self
    }

    /// Stable Catalog Agent UUID.
    pub fn agent_id(mut self, v: impl Into<String>) -> Self {
        self.config.agent_id = v.into();
        self
    }

    /// Stable external Binding UUID.
    pub fn binding_id(mut self, v: impl Into<String>) -> Self {
        self.config.binding_id = v.into();
        self
    }

    /// Registered AgentInstance generation.
    pub fn generation(mut self, v: i64) -> Self {
        self.config.generation = v;
        self
    }

    /// Collaboration tenant.
    pub fn tenant(mut self, v: impl Into<String>) -> Self {
        self.config.tenant = v.into();
        self
    }

    /// Collaboration / Kubernetes namespace.
    pub fn namespace(mut self, v: impl Into<String>) -> Self {
        self.config.namespace = v.into();
        self
    }

    /// This replica's stable key.
    pub fn instance_key(mut self, v: impl Into<String>) -> Self {
        self.config.instance_key = v.into();
        self
    }

    /// Whether to persist and push the Level-2 event stream.
    pub fn enable_events(mut self, v: bool) -> Self {
        self.config.enable_events = v;
        self
    }

    /// Durable event outbox directory.
    pub fn event_journal_dir(mut self, v: impl Into<String>) -> Self {
        self.config.event_journal_dir = v.into();
        self
    }

    /// In-process contract server port (`0` = ephemeral).
    pub fn contract_http_port(mut self, v: u16) -> Self {
        self.config.contract_http_port = v;
        self
    }

    /// Contract server bind address.
    pub fn contract_http_host(mut self, v: impl Into<String>) -> Self {
        self.config.contract_http_host = v.into();
        self
    }

    /// Public base URL the control plane should use.
    pub fn public_base_url(mut self, v: impl Into<String>) -> Self {
        self.config.public_base_url = v.into();
        self
    }

    /// Session affinity hint.
    pub fn session_affinity(mut self, v: impl Into<String>) -> Self {
        self.config.session_affinity = v.into();
        self
    }

    /// Whether to start the contract server.
    pub fn start_http(mut self, v: bool) -> Self {
        self.config.start_http = v;
        self
    }

    /// Whether to open the ASDP upstream channel.
    pub fn start_grpc(mut self, v: bool) -> Self {
        self.config.start_grpc = v;
        self
    }

    /// Whether to POST `/api/v1/agent-registrations`.
    pub fn start_http_register(mut self, v: bool) -> Self {
        self.config.start_http_register = v;
        self
    }

    /// Builds and validates the config.
    pub fn build(self) -> ControlPlaneResult<ControlPlaneConfig> {
        self.config.validate()
    }
}

/// Default instance id: `HOSTNAME` env var, falling back to `"local"`.
fn default_instance_id() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "local".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_agent_key() {
        assert!(ControlPlaneConfig::builder("  ").build().is_err());
    }

    #[test]
    fn defaults_tenant_and_namespace() {
        let cfg = ControlPlaneConfig::builder("friday")
            .tenant("")
            .namespace("")
            .instance_key("rep-1")
            .build()
            .unwrap();
        assert_eq!(cfg.tenant, DEFAULT_TENANT);
        assert_eq!(cfg.namespace, DEFAULT_NAMESPACE);
        assert_eq!(cfg.instance_key, "rep-1");
    }

    #[test]
    fn grpc_requires_control_plane() {
        let err = ControlPlaneConfig::builder("friday").start_grpc(true).build();
        assert!(err.is_err());
    }

    #[test]
    fn grpc_without_register_requires_identity() {
        let err = ControlPlaneConfig::builder("friday")
            .start_grpc(true)
            .control_plane("localhost:9090")
            .build();
        assert!(err.is_err());
        let ok = ControlPlaneConfig::builder("friday")
            .start_grpc(true)
            .control_plane("localhost:9090")
            .agent_id("a")
            .binding_id("b")
            .generation(1)
            .build();
        assert!(ok.is_ok());
    }
}
