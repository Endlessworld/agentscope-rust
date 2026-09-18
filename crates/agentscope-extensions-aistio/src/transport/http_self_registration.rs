//! v5 external-application registration against aistiod, mirroring
//! `io.agentscope.extensions.aistio.transport.HttpSelfRegistration`.
//!
//! `POST /api/v1/agent-registrations` establishes a stable identity (agent / binding /
//! instance + credential); instance heartbeats then keep it alive
//! (`POST /api/v1/dataplanes/{id}/heartbeat`) and a 404 triggers re-registration.
//! Failures are swallowed and retried — registration must never disturb the agent.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{AistioError, AistioResult};
use crate::transport::control_plane_http_client::ControlPlaneHttpClient;

/// Header carrying the claim/issued registration credential.
pub const REGISTRATION_CREDENTIAL_HEADER: &str = "X-Agent-Registration-Credential";

/// Default heartbeat interval, matching the Java `HttpSelfRegistrationTest` expectation.
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Stable identity returned by the Catalog registration transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisteredIdentity {
    /// Stable Catalog Agent UUID.
    pub agent_id: String,
    /// Logical agent key registered in the control plane.
    pub agent_key: String,
    /// Stable external Binding UUID.
    pub binding_id: String,
    /// Durable AgentInstance UUID used for heartbeats.
    pub instance_id: String,
    /// This replica's stable key.
    pub instance_key: String,
    /// Registered AgentInstance generation.
    pub generation: i64,
    /// Credential issued (or echoed) by the control plane.
    pub registration_credential: String,
}

/// Connection + identity settings for [`HttpSelfRegistration`].
#[derive(Debug, Clone)]
pub struct RegistrationOptions {
    /// Registration credential claimed on first registration (may be empty).
    pub registration_credential: String,
    /// Logical agent key (required).
    pub agent_key: String,
    /// Collaboration tenant; blank falls back to `default`.
    pub tenant: String,
    /// Collaboration / Kubernetes namespace; blank falls back to `default`.
    pub namespace: String,
    /// This replica's stable key (required).
    pub instance_key: String,
    /// URL the control plane should use to reach the `/agentscope/*` contract.
    pub base_url: String,
    /// SDK runtime version reported as `sdkVersion`.
    pub runtime: String,
    /// Framework identifier reported as `framework`.
    pub framework: String,
    /// Contract level served by this instance.
    pub contract_level: u32,
    /// Capability identifiers declared by the adapter.
    pub capabilities: Vec<String>,
    /// Heartbeat interval.
    pub heartbeat_interval: Duration,
}

impl Default for RegistrationOptions {
    fn default() -> Self {
        Self {
            registration_credential: String::new(),
            agent_key: String::new(),
            tenant: "default".into(),
            namespace: "default".into(),
            instance_key: String::new(),
            base_url: String::new(),
            runtime: String::new(),
            framework: String::new(),
            contract_level: 0,
            capabilities: Vec::new(),
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
        }
    }
}

impl RegistrationOptions {
    /// Creates options for the given logical identity.
    pub fn new(agent_key: impl Into<String>, instance_key: impl Into<String>) -> Self {
        Self {
            agent_key: agent_key.into(),
            instance_key: instance_key.into(),
            ..Default::default()
        }
    }

    /// Sets the contract base URL the control plane should poll.
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Sets the framework identifier.
    pub fn framework(mut self, framework: impl Into<String>) -> Self {
        self.framework = framework.into();
        self
    }

    /// Sets the SDK runtime version.
    pub fn runtime(mut self, runtime: impl Into<String>) -> Self {
        self.runtime = runtime.into();
        self
    }

    /// Sets the declared capabilities.
    pub fn capabilities(mut self, capabilities: Vec<String>) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Sets the tenant.
    pub fn tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = tenant.into();
        self
    }

    /// Sets the namespace.
    pub fn namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = namespace.into();
        self
    }

    /// Sets the claimed registration credential.
    pub fn registration_credential(mut self, credential: impl Into<String>) -> Self {
        self.registration_credential = credential.into();
        self
    }

    /// Sets the heartbeat interval.
    pub fn heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval;
        self
    }

    /// Validates + normalizes, mirroring the Java canonical constructor.
    pub fn validate(mut self) -> AistioResult<Self> {
        if self.agent_key.trim().is_empty() {
            return Err(AistioError::Config("agentKey is required".into()));
        }
        if self.instance_key.trim().is_empty() {
            return Err(AistioError::Config("instanceKey is required".into()));
        }
        if self.tenant.trim().is_empty() {
            self.tenant = "default".into();
        }
        if self.namespace.trim().is_empty() {
            self.namespace = "default".into();
        }
        self.registration_credential = self.registration_credential.trim().to_string();
        self.base_url = crate::transport::trim_slash(&self.base_url);
        Ok(self)
    }

    /// Builds the `POST /api/v1/agent-registrations` request body (pure).
    pub fn registration_body(&self) -> Value {
        json!({
            "tenant": self.tenant,
            "agentKey": self.agent_key,
            "namespace": self.namespace,
            "instanceKey": self.instance_key,
            "routingKey": self.base_url,
            "framework": self.framework,
            "sdkVersion": self.runtime,
            "capacity": 1,
            "capabilities": self.capabilities,
        })
    }
}

/// Outcome of a single registration attempt.
#[derive(Debug, Clone)]
enum RegisterAttempt {
    Registered(Box<RegisteredIdentity>),
    /// Non-2xx status returned by the control plane.
    Status(u16),
}

/// Parses the registration transaction document (pure, unit-testable).
///
/// Returns an error when any stable identity element or the credential is missing.
pub fn parse_registered_identity(
    body: &str,
    agent_key: &str,
    instance_key: &str,
    claim_credential: &str,
) -> AistioResult<RegisteredIdentity> {
    let doc: Value = serde_json::from_str(body)
        .map_err(|e| AistioError::Http(format!("registration response is not JSON: {e}")))?;
    let agent_id = doc.pointer("/agent/id").and_then(|v| v.as_str()).unwrap_or("");
    let binding_id = doc.pointer("/binding/id").and_then(|v| v.as_str()).unwrap_or("");
    let instance_id = doc.pointer("/instance/id").and_then(|v| v.as_str()).unwrap_or("");
    let generation = doc.pointer("/instance/generation").and_then(|v| v.as_i64()).unwrap_or(0);
    let issued = doc
        .get("registrationCredential")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(claim_credential);
    if agent_id.trim().is_empty()
        || binding_id.trim().is_empty()
        || instance_id.trim().is_empty()
        || generation <= 0
        || issued.trim().is_empty()
    {
        return Err(AistioError::Config(
            "registration response is missing stable identity or credential".into(),
        ));
    }
    Ok(RegisteredIdentity {
        agent_id: agent_id.to_string(),
        agent_key: agent_key.to_string(),
        binding_id: binding_id.to_string(),
        instance_id: instance_id.to_string(),
        instance_key: instance_key.to_string(),
        generation,
        registration_credential: issued.to_string(),
    })
}

/// Identity-change listener.
pub type IdentityListener = Arc<dyn Fn(&RegisteredIdentity) + Send + Sync>;

/// v5 external-application registration + periodic instance heartbeats.
pub struct HttpSelfRegistration {
    http: Arc<ControlPlaneHttpClient>,
    options: RegistrationOptions,
    registered: AtomicBool,
    registered_instance_id: RwLock<Option<String>>,
    generation: AtomicI64,
    identity: RwLock<Option<RegisteredIdentity>>,
    listener: Mutex<Option<IdentityListener>>,
}

impl HttpSelfRegistration {
    /// Creates a registration client (does not contact the control plane yet).
    pub fn new(
        control_plane_http: impl Into<String>,
        internal_token: impl Into<String>,
        options: RegistrationOptions,
    ) -> AistioResult<Self> {
        let options = options.validate()?;
        Ok(Self {
            http: Arc::new(ControlPlaneHttpClient::new(control_plane_http, internal_token)?),
            options,
            registered: AtomicBool::new(false),
            registered_instance_id: RwLock::new(None),
            generation: AtomicI64::new(0),
            identity: RwLock::new(None),
            listener: Mutex::new(None),
        })
    }

    /// The connection options (already normalized).
    pub fn options(&self) -> &RegistrationOptions {
        &self.options
    }

    /// Whether this instance is currently registered.
    pub fn is_registered(&self) -> bool {
        self.registered.load(Ordering::SeqCst)
    }

    /// The last successfully registered identity, if any.
    pub fn identity(&self) -> Option<RegisteredIdentity> {
        self.identity.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Receives every successfully refreshed identity, including generation changes.
    pub fn set_identity_listener(&self, listener: Option<IdentityListener>) {
        *self.listener.lock().unwrap_or_else(|p| p.into_inner()) = listener;
    }

    fn notify(&self, identity: &RegisteredIdentity) {
        let listener = self
            .listener
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(l) = listener {
            l(identity);
        }
    }

    fn claim_credential(&self) -> String {
        self.identity()
            .map(|i| i.registration_credential)
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| self.options.registration_credential.clone())
    }

    async fn attempt_register(&self) -> AistioResult<RegisterAttempt> {
        let credential = self.claim_credential();
        let headers: Vec<(&str, &str)> = if credential.trim().is_empty() {
            Vec::new()
        } else {
            vec![(REGISTRATION_CREDENTIAL_HEADER, credential.as_str())]
        };
        let resp = self
            .http
            .send_with_headers(
                Method::POST,
                "/api/v1/agent-registrations",
                Some(self.options.registration_body()),
                &headers,
            )
            .await?;
        if !(200..300).contains(&resp.status) {
            return Ok(RegisterAttempt::Status(resp.status));
        }
        let identity = parse_registered_identity(
            &resp.body,
            &self.options.agent_key,
            &self.options.instance_key,
            &credential,
        )?;
        Ok(RegisterAttempt::Registered(Box::new(identity)))
    }

    /// Performs one registration attempt, updating state on success.
    ///
    /// This is the synchronous-by-contract entry the heartbeat loop calls; failures are
    /// returned but callers may ignore them (registration must never disturb the agent).
    pub async fn register_once(&self) -> AistioResult<Option<RegisteredIdentity>> {
        match self.attempt_register().await {
            Ok(RegisterAttempt::Registered(identity)) => {
                let identity = *identity;
                *self
                    .registered_instance_id
                    .write()
                    .unwrap_or_else(|p| p.into_inner()) = Some(identity.instance_id.clone());
                self.generation.store(identity.generation, Ordering::SeqCst);
                *self.identity.write().unwrap_or_else(|p| p.into_inner()) = Some(identity.clone());
                self.registered.store(true, Ordering::SeqCst);
                self.notify(&identity);
                Ok(Some(identity))
            }
            Ok(RegisterAttempt::Status(status)) => {
                self.registered.store(false, Ordering::SeqCst);
                Err(AistioError::ControlPlane {
                    status,
                    message: "register".into(),
                })
            }
            Err(e) => {
                self.registered.store(false, Ordering::SeqCst);
                Err(e)
            }
        }
    }

    /// Sends one heartbeat; a 404 marks the instance unregistered.
    pub async fn heartbeat_once(&self) -> AistioResult<u16> {
        let instance_id = self
            .registered_instance_id
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let Some(instance_id) = instance_id else {
            self.registered.store(false, Ordering::SeqCst);
            return Ok(404);
        };
        let path = format!(
            "/api/v1/dataplanes/{}/heartbeat",
            crate::transport::encode_path_segment(&instance_id)
        );
        let resp = self
            .http
            .send(
                Method::POST,
                &path,
                Some(json!({ "generation": self.generation.load(Ordering::SeqCst) })),
            )
            .await?;
        if resp.status == 404 {
            self.registered.store(false, Ordering::SeqCst);
            *self
                .registered_instance_id
                .write()
                .unwrap_or_else(|p| p.into_inner()) = None;
        }
        Ok(resp.status)
    }

    /// Attempts re-registration, swallowing failures (mirrors `tryRegister`).
    pub async fn try_register(&self) -> Option<RegisteredIdentity> {
        self.register_once().await.ok().flatten()
    }

    /// Unregisters the instance (`DELETE /api/v1/dataplanes/{id}`) and clears state.
    pub async fn unregister(&self) -> AistioResult<()> {
        let instance_id = self
            .registered_instance_id
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let result = if let (true, Some(id)) = (self.is_registered(), instance_id) {
            let path = format!(
                "/api/v1/dataplanes/{}",
                crate::transport::encode_path_segment(&id)
            );
            self.http
                .send(
                    Method::DELETE,
                    &path,
                    Some(json!({ "generation": self.generation.load(Ordering::SeqCst) })),
                )
                .await
                .map(|_| ())
        } else {
            Ok(())
        };
        self.registered.store(false, Ordering::SeqCst);
        *self
            .registered_instance_id
            .write()
            .unwrap_or_else(|p| p.into_inner()) = None;
        result
    }

    /// Registers immediately and spawns the heartbeat loop on the current tokio runtime.
    ///
    /// Calling outside a tokio runtime only performs the initial registration.
    pub async fn start(self: &Arc<Self>) {
        let _ = self.register_once().await;
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let interval = self.options.heartbeat_interval.max(Duration::from_millis(1));
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // first tick fires immediately
            loop {
                ticker.tick().await;
                if !this.is_registered() {
                    let _ = this.try_register().await;
                    continue;
                }
                match this.heartbeat_once().await {
                    Ok(404) => {
                        let _ = this.try_register().await;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        let _ = this.try_register().await;
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> RegistrationOptions {
        RegistrationOptions::new("friday", "rep-1")
            .base_url("http://localhost:9090/")
            .framework("agentscope-rust")
            .runtime("0.1.0")
            .capabilities(vec!["context-query".into()])
    }

    #[test]
    fn validate_requires_identity_and_trims_base_url() {
        let opts = RegistrationOptions::new("", "rep-1").validate();
        assert!(opts.is_err());
        let opts = RegistrationOptions::new("friday", "  ").validate();
        assert!(opts.is_err());
        let opts = options().validate().unwrap();
        assert_eq!(opts.base_url, "http://localhost:9090");
    }

    #[test]
    fn registration_body_matches_contract() {
        let body = options().validate().unwrap().registration_body();
        assert_eq!(body["agentKey"], "friday");
        assert_eq!(body["instanceKey"], "rep-1");
        assert_eq!(body["routingKey"], "http://localhost:9090");
        assert_eq!(body["sdkVersion"], "0.1.0");
        assert_eq!(body["capacity"], 1);
        assert_eq!(body["capabilities"][0], "context-query");
    }

    #[test]
    fn parses_identity_document() {
        let doc = r#"{
            "agent": {"id": "a-1"},
            "binding": {"id": "b-1"},
            "instance": {"id": "i-1", "generation": 7},
            "registrationCredential": "cred-x"
        }"#;
        let id = parse_registered_identity(doc, "friday", "rep-1", "claim").unwrap();
        assert_eq!(id.agent_id, "a-1");
        assert_eq!(id.binding_id, "b-1");
        assert_eq!(id.instance_id, "i-1");
        assert_eq!(id.generation, 7);
        assert_eq!(id.registration_credential, "cred-x");
    }

    #[test]
    fn falls_back_to_claim_credential() {
        let doc = r#"{
            "agent": {"id": "a-1"},
            "binding": {"id": "b-1"},
            "instance": {"id": "i-1", "generation": 1}
        }"#;
        let id = parse_registered_identity(doc, "friday", "rep-1", "claim").unwrap();
        assert_eq!(id.registration_credential, "claim");
    }

    #[test]
    fn rejects_incomplete_identity() {
        let doc = r#"{"agent": {"id": "a-1"}, "instance": {"generation": 0}}"#;
        assert!(parse_registered_identity(doc, "friday", "rep-1", "").is_err());
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_registered_identity("not json", "a", "b", "c").is_err());
    }
}
