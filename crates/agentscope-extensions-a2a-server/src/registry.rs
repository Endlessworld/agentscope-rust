//! Agent registration and discovery, mirroring
//! `io.agentscope.core.a2a.server.registry`.
//!
//! Java wires this through `AgentScopeA2aServer#postEndpointReady()`, which calls
//! [`AgentRegistryService::register`] once every endpoint is reachable. Registries are the
//! integration point for service discovery backends (Nacos and friends): a backend crate
//! implements [`AgentRegistry`] and is handed to the server builder with
//! `with_agent_registry(...)`.

use std::sync::Arc;

use agentscope_extensions_a2a_client::model::AgentCard;

use crate::card::TransportProperties;

/// Failure returned by an [`AgentRegistry`] implementation.
///
/// Java's `register` is `void` and may throw any exception; Rust keeps the error so
/// [`AgentRegistryService`] can report per-registry failures without aborting the rest.
pub type AgentRegistryError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Agent registration interface to an agent registry.
///
/// Mirrors Java `io.agentscope.core.a2a.server.registry.AgentRegistry`.
pub trait AgentRegistry: Send + Sync {
    /// Registry name, used for logging and failure reports.
    fn registry_name(&self) -> String;

    /// Registers `agent_card` with the transports this server actually exposes.
    fn register(
        &self,
        agent_card: &AgentCard,
        transport_properties: &[TransportProperties],
    ) -> Result<(), AgentRegistryError>;
}

/// The outcome of one [`AgentRegistryService::register`] sweep.
///
/// Java only logs successes and failures
/// (`AgentRegistryService#register`); the report keeps the same "one failing registry never
/// blocks the others" contract while staying observable from Rust callers and tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistrationReport {
    /// Names of the registries that accepted the agent, in registration order.
    pub succeeded: Vec<String>,
    /// `(registry name, error message)` for the registries that failed.
    pub failed: Vec<(String, String)>,
}

impl RegistrationReport {
    /// Whether no registry was attempted (no card or no transport).
    pub fn is_empty(&self) -> bool {
        self.succeeded.is_empty() && self.failed.is_empty()
    }

    /// Number of registries that accepted the agent.
    pub fn succeeded_count(&self) -> usize {
        self.succeeded.len()
    }
}

/// Registers the served agent with every configured [`AgentRegistry`].
///
/// Mirrors Java `io.agentscope.core.a2a.server.registry.AgentRegistryService`: a failure in one
/// registry is recorded and does not prevent the remaining registries from running.
#[derive(Clone, Default)]
pub struct AgentRegistryService {
    agent_registries: Vec<Arc<dyn AgentRegistry>>,
}

impl std::fmt::Debug for AgentRegistryService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRegistryService")
            .field("registries", &self.agent_registries.len())
            .finish()
    }
}

impl AgentRegistryService {
    /// Creates a service over the given registries.
    pub fn new(agent_registries: Vec<Arc<dyn AgentRegistry>>) -> Self {
        Self { agent_registries }
    }

    /// The configured registries.
    pub fn registries(&self) -> &[Arc<dyn AgentRegistry>] {
        &self.agent_registries
    }

    /// Whether any registry is configured.
    pub fn is_empty(&self) -> bool {
        self.agent_registries.is_empty()
    }

    /// Registers `agent_card` with every registry.
    ///
    /// Returns an empty report when the agent exposes no transport, mirroring Java's early
    /// return for a null card or an empty transport set.
    pub fn register(
        &self,
        agent_card: &AgentCard,
        transport_properties: &[TransportProperties],
    ) -> RegistrationReport {
        let mut report = RegistrationReport::default();
        if transport_properties.is_empty() {
            return report;
        }
        for registry in &self.agent_registries {
            let name = registry.registry_name();
            tracing::info!(
                agent = agent_card.name.as_str(),
                registry = name.as_str(),
                "auto register agent into registry"
            );
            match registry.register(agent_card, transport_properties) {
                Ok(()) => report.succeeded.push(name),
                Err(error) => {
                    tracing::error!(
                        agent = agent_card.name.as_str(),
                        registry = name.as_str(),
                        error = error.to_string().as_str(),
                        "auto register agent into registry failed"
                    );
                    report.failed.push((name, error.to_string()));
                }
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct RecordingRegistry {
        name: String,
        fail: bool,
        calls: Mutex<Vec<(String, usize)>>,
    }

    impl RecordingRegistry {
        fn new(name: &str, fail: bool) -> Self {
            Self {
                name: name.to_string(),
                fail,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl AgentRegistry for RecordingRegistry {
        fn registry_name(&self) -> String {
            self.name.clone()
        }

        fn register(
            &self,
            agent_card: &AgentCard,
            transport_properties: &[TransportProperties],
        ) -> Result<(), AgentRegistryError> {
            self.calls
                .lock()
                .unwrap()
                .push((agent_card.name.clone(), transport_properties.len()));
            if self.fail {
                return Err(format!("{} is down", self.name).into());
            }
            Ok(())
        }
    }

    fn card() -> AgentCard {
        AgentCard::new("agent", "http://localhost:8080")
    }

    fn transports() -> Vec<TransportProperties> {
        vec![TransportProperties::builder("JSONRPC")
            .host("localhost")
            .port(8080)
            .build()]
    }

    #[test]
    fn register_reaches_every_registry_with_the_served_transports() {
        let first = Arc::new(RecordingRegistry::new("first", false));
        let second = Arc::new(RecordingRegistry::new("second", false));
        let service = AgentRegistryService::new(vec![first.clone(), second.clone()]);

        let report = service.register(&card(), &transports());

        assert_eq!(
            report.succeeded,
            vec!["first".to_string(), "second".to_string()]
        );
        assert!(report.failed.is_empty());
        assert_eq!(report.succeeded_count(), 2);
        assert_eq!(
            first.calls.lock().unwrap().as_slice(),
            &[("agent".to_string(), 1)]
        );
        assert_eq!(
            second.calls.lock().unwrap().as_slice(),
            &[("agent".to_string(), 1)]
        );
    }

    #[test]
    fn one_failing_registry_never_blocks_the_others() {
        let broken = Arc::new(RecordingRegistry::new("broken", true));
        let healthy = Arc::new(RecordingRegistry::new("healthy", false));
        let service = AgentRegistryService::new(vec![broken, healthy.clone()]);

        let report = service.register(&card(), &transports());

        assert_eq!(report.succeeded, vec!["healthy".to_string()]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "broken");
        assert_eq!(report.failed[0].1, "broken is down");
        assert_eq!(healthy.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn no_transport_means_no_registry_call() {
        let registry = Arc::new(RecordingRegistry::new("only", false));
        let service = AgentRegistryService::new(vec![registry.clone()]);

        let report = service.register(&card(), &[]);

        assert!(report.is_empty());
        assert!(registry.calls.lock().unwrap().is_empty());
    }
}
