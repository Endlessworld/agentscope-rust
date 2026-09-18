//! Kubernetes (agent-sandbox CRD) sandbox backend.
//!
//! Mirrors the Java `io.agentscope.extensions.sandbox.kubernetes` package: sandbox provisioning
//! through the `extensions.agents.x-k8s.io/v1beta1` `SandboxClaim` CRD, plus command execution
//! against the sandbox server's `POST /execute` endpoint.
//!
//! The Java client supports four connection strategies (direct / gateway / in-cluster /
//! port-forward). Rust implements the **gateway/direct** strategy via a resolved base URL; the
//! in-cluster service-account auto-detection and the port-forward strategy are recorded as a
//! P2 gap in the module tracker.

use std::collections::BTreeMap;
use std::sync::Arc;

use reqwest::{Client, Response};
use serde_json::{json, Value};

use crate::error::{SandboxErrorCode, SandboxExtensionError, SandboxResult};
use crate::http::join_url;
use crate::remote::{
    RemoteBackend, RemoteExecOutput, RemoteSandboxClient, RemoteSandboxHandle,
};
use crate::retry::{with_retries, RetryPolicy};

/// CRD group for `SandboxClaim`.
pub const CLAIM_API_GROUP: &str = "extensions.agents.x-k8s.io";
/// CRD version for `SandboxClaim`.
pub const CLAIM_API_VERSION: &str = "v1beta1";
/// CRD plural name for `SandboxClaim`.
pub const CLAIM_PLURAL_NAME: &str = "sandboxclaims";
/// Label recording the creating client.
pub const CREATED_BY_LABEL: &str = "agents.x-k8s.io/created-by";
/// Value used for [`CREATED_BY_LABEL`].
pub const CREATED_BY_VALUE: &str = "agentscope-rust-client";
/// Default sandbox server port.
pub const DEFAULT_SERVER_PORT: u16 = 8888;
/// Default workspace root.
pub const DEFAULT_WORKSPACE_ROOT: &str = "/workspace";
/// Readiness poll interval, in milliseconds.
pub const READY_POLL_INTERVAL_MS: u64 = 1500;

/// Options for the Kubernetes sandbox client (mirrors `KubernetesSandboxClientOptions`).
#[derive(Debug, Clone)]
pub struct KubernetesSandboxClientOptions {
    /// Kubernetes namespace.
    pub namespace: String,
    /// Warm-pool name referenced by the claim.
    pub warm_pool_name: Option<String>,
    /// Absolute workspace root inside the sandbox.
    pub workspace_root: String,
    /// Base directory used by the sandbox file API.
    pub file_api_base_dir: String,
    /// Kubernetes API server URL (e.g. `https://kubernetes.default.svc`).
    pub api_url: Option<String>,
    /// Bearer token for the Kubernetes API.
    pub api_token: Option<String>,
    /// Gateway name used to derive the connector base URL.
    pub gateway_name: Option<String>,
    /// Gateway namespace.
    pub gateway_namespace: Option<String>,
    /// Gateway scheme (`http`/`https`).
    pub gateway_scheme: String,
    /// Sandbox server port.
    pub server_port: u16,
    /// Timeout for the sandbox to become ready, in seconds.
    pub sandbox_ready_timeout_seconds: u64,
    /// TCP connect timeout in seconds.
    pub connect_timeout_seconds: u64,
    /// Read timeout in seconds.
    pub read_timeout_seconds: u64,
    /// Maximum retry attempts.
    pub max_retries: u32,
}

impl Default for KubernetesSandboxClientOptions {
    fn default() -> Self {
        Self {
            namespace: "default".to_string(),
            warm_pool_name: None,
            workspace_root: DEFAULT_WORKSPACE_ROOT.to_string(),
            file_api_base_dir: DEFAULT_WORKSPACE_ROOT.to_string(),
            api_url: None,
            api_token: None,
            gateway_name: None,
            gateway_namespace: None,
            gateway_scheme: "http".to_string(),
            server_port: DEFAULT_SERVER_PORT,
            sandbox_ready_timeout_seconds: 180,
            connect_timeout_seconds: 30,
            read_timeout_seconds: 180,
            max_retries: 3,
        }
    }
}

/// Builds the `SandboxClaim` CRD body for a deterministic claim name.
pub fn build_claim_body(
    namespace: &str,
    claim_name: &str,
    warm_pool_name: &str,
) -> Value {
    json!({
        "apiVersion": format!("{CLAIM_API_GROUP}/{CLAIM_API_VERSION}"),
        "kind": "SandboxClaim",
        "metadata": {
            "name": claim_name,
            "namespace": namespace,
            "labels": { CREATED_BY_LABEL: CREATED_BY_VALUE },
        },
        "spec": {
            "warmPoolRef": { "name": warm_pool_name }
        }
    })
}

/// Builds the sandbox server `POST /execute` request body.
pub fn build_execute_body(command: &str) -> Value {
    json!({ "command": command })
}

/// Result of a sandbox-server command execution.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KubernetesExecResult {
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Exit code (defaults to -1 when absent).
    pub exit_code: i32,
}

/// Parses a sandbox-server `ExecutionResult` response.
pub fn parse_execute_result(value: &Value) -> KubernetesExecResult {
    KubernetesExecResult {
        stdout: value
            .get("stdout")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        stderr: value
            .get("stderr")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        exit_code: value.get("exitCode").and_then(Value::as_i64).unwrap_or(-1) as i32,
    }
}

/// Provisioning fields extracted from a `SandboxClaim` resource.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KubernetesClaimStatus {
    /// Sandbox resource name (`status.sandbox.name`).
    pub sandbox_name: Option<String>,
    /// Pod IPs (`status.sandbox.podIPs`).
    pub pod_ips: Vec<String>,
    /// Whether a `Ready` condition is `True`.
    pub ready: bool,
    /// Reason of the first non-ready condition, when present.
    pub reason: Option<String>,
}

/// Parses a `SandboxClaim` resource's status.
pub fn parse_claim_status(value: &Value) -> KubernetesClaimStatus {
    let status = value.get("status").cloned().unwrap_or(Value::Null);
    let sandbox = status.get("sandbox").cloned().unwrap_or(Value::Null);
    let sandbox_name = sandbox
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string());
    let pod_ips = sandbox
        .get("podIPs")
        .and_then(Value::as_array)
        .map(|ips| {
            ips.iter()
                .filter_map(Value::as_str)
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    let mut ready = false;
    let mut reason = None;
    if let Some(conditions) = status.get("conditions").and_then(Value::as_array) {
        for condition in conditions {
            let ctype = condition.get("type").and_then(Value::as_str).unwrap_or("");
            let cstatus = condition.get("status").and_then(Value::as_str).unwrap_or("");
            if ctype.eq_ignore_ascii_case("ready") && cstatus.eq_ignore_ascii_case("true") {
                ready = true;
            }
            if reason.is_none() && !cstatus.eq_ignore_ascii_case("true") {
                reason = condition
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| condition.get("reason").and_then(Value::as_str))
                    .map(|s| s.to_string());
            }
        }
    }
    KubernetesClaimStatus { sandbox_name, pod_ips, ready, reason }
}

/// Resolves the sandbox server base URL from a pod IP (direct strategy) or the gateway.
pub fn resolve_connector_base_url(
    options: &KubernetesSandboxClientOptions,
    pod_ip: Option<&str>,
) -> SandboxResult<String> {
    if let Some(pod_ip) = pod_ip.filter(|ip| !ip.trim().is_empty()) {
        return Ok(format!("http://{pod_ip}:{}", options.server_port));
    }
    match (
        options.gateway_name.as_deref().filter(|g| !g.trim().is_empty()),
        options
            .gateway_namespace
            .as_deref()
            .filter(|g| !g.trim().is_empty()),
    ) {
        (Some(name), Some(namespace)) => Ok(format!(
            "{}://{name}.{namespace}.svc.cluster.local:{}",
            options.gateway_scheme, options.server_port
        )),
        _ => Err(SandboxExtensionError::Configuration(
            "Kubernetes sandbox needs a pod IP or gateway name+namespace to reach the sandbox server"
                .to_string(),
        )),
    }
}

/// Kubernetes [`RemoteBackend`] implementation.
#[derive(Debug)]
pub struct KubernetesBackend {
    options: KubernetesSandboxClientOptions,
    client: Client,
    retry: RetryPolicy,
}

impl KubernetesBackend {
    /// Builds a backend, wiring up the reqwest client and retry policy.
    pub fn new(options: KubernetesSandboxClientOptions) -> SandboxResult<Self> {
        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(options.connect_timeout_seconds))
            .timeout(std::time::Duration::from_secs(options.read_timeout_seconds))
            .build()
            .map_err(|e| SandboxExtensionError::Transport(format!("failed to build client: {e}")))?;
        let retry = RetryPolicy::kubernetes(options.max_retries);
        Ok(Self { options, client, retry })
    }

    /// Returns the configured options.
    pub fn options(&self) -> &KubernetesSandboxClientOptions {
        &self.options
    }

    fn claims_collection_path(&self) -> String {
        format!(
            "/apis/{CLAIM_API_GROUP}/{CLAIM_API_VERSION}/namespaces/{}/{CLAIM_PLURAL_NAME}",
            self.options.namespace
        )
    }

    fn claim_path(&self, claim_name: &str) -> String {
        format!("{}/{claim_name}", self.claims_collection_path())
    }

    fn api_base(&self) -> SandboxResult<String> {
        self.options
            .api_url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .ok_or_else(|| {
                SandboxExtensionError::Configuration(
                    "Kubernetes API URL is required (KubernetesSandboxClientOptions::api_url)"
                        .to_string(),
                )
            })
    }

    async fn decode_json(response: Response, url: &str) -> SandboxResult<Value> {
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(SandboxExtensionError::http(status.as_u16(), text));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| {
            SandboxExtensionError::Serialization(format!("invalid JSON from {url}: {e}"))
        })
    }

    async fn k8s_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> SandboxResult<Value> {
        let base = self.api_base()?;
        let url = join_url(&base, path);
        let token = self.options.api_token.clone();
        let client = self.client.clone();
        let retry = self.retry.clone();
        with_retries(&retry, move || {
            let client = client.clone();
            let url = url.clone();
            let method = method.clone();
            let token = token.clone();
            let body = body.clone();
            async move {
                let mut request = client.request(method, &url);
                if let Some(token) = token.filter(|t| !t.trim().is_empty()) {
                    request = request.bearer_auth(token);
                }
                if let Some(body) = &body {
                    request = request.json(body);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
                Self::decode_json(response, &url).await
            }
        })
        .await
    }

    async fn wait_until_ready(&self, claim_name: &str) -> SandboxResult<KubernetesClaimStatus> {
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(self.options.sandbox_ready_timeout_seconds.max(1));
        let mut last_reason: Option<String> = None;
        loop {
            let path = self.claim_path(claim_name);
            if let Ok(value) = self.k8s_request(reqwest::Method::GET, &path, None).await {
                let status = parse_claim_status(&value);
                if status.ready && status.sandbox_name.is_some() {
                    return Ok(status);
                }
                if status.reason.is_some() {
                    last_reason = status.reason.clone();
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(SandboxExtensionError::Runtime(
                    SandboxErrorCode::WorkspaceStartError,
                    format!(
                        "Kubernetes sandbox claim did not become ready: {claim_name} (last reason={})",
                        last_reason.unwrap_or_else(|| "none".to_string())
                    ),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(READY_POLL_INTERVAL_MS)).await;
        }
    }

    fn handle_from_status(&self, claim: &str, status: &KubernetesClaimStatus) -> RemoteSandboxHandle {
        let mut extra = BTreeMap::new();
        extra.insert("claim_name".to_string(), claim.to_string());
        extra.insert("namespace".to_string(), self.options.namespace.clone());
        if let Some(name) = &status.sandbox_name {
            extra.insert("sandbox_name".to_string(), name.clone());
        }
        if let Some(ip) = status.pod_ips.first() {
            extra.insert("pod_ip".to_string(), ip.clone());
        }
        RemoteSandboxHandle::new(claim.to_string(), extra)
    }
}

#[async_trait::async_trait]
impl RemoteBackend for KubernetesBackend {
    fn backend_name(&self) -> &'static str {
        "kubernetes"
    }

    fn endpoint_base(&self) -> String {
        self.options.api_url.clone().unwrap_or_default()
    }

    fn workspace_root(&self) -> &str {
        &self.options.workspace_root
    }

    async fn create_sandbox(&self, session_id: &str) -> SandboxResult<RemoteSandboxHandle> {
        let warm_pool = self
            .options
            .warm_pool_name
            .clone()
            .filter(|w| !w.trim().is_empty())
            .ok_or_else(|| {
                SandboxExtensionError::Configuration(
                    "Kubernetes warm pool name is required (KubernetesSandboxClientOptions::warm_pool_name)"
                        .to_string(),
                )
            })?;
        let claim_name = format!("agentscope-{session_id}");
        let collection = self.claims_collection_path();
        let body = build_claim_body(&self.options.namespace, &claim_name, &warm_pool);
        self.k8s_request(reqwest::Method::POST, &collection, Some(body)).await?;
        let status = self.wait_until_ready(&claim_name).await?;
        Ok(self.handle_from_status(&claim_name, &status))
    }

    async fn connect_sandbox(&self, sandbox_id: &str) -> SandboxResult<RemoteSandboxHandle> {
        let path = self.claim_path(sandbox_id);
        let value = self.k8s_request(reqwest::Method::GET, &path, None).await?;
        let status = parse_claim_status(&value);
        Ok(self.handle_from_status(sandbox_id, &status))
    }

    async fn kill_sandbox(&self, sandbox_id: &str) -> SandboxResult<()> {
        let path = self.claim_path(sandbox_id);
        match self
            .k8s_request(reqwest::Method::DELETE, &path, None)
            .await
        {
            Ok(_) => Ok(()),
            Err(SandboxExtensionError::Http { status: 404, .. }) => Ok(()),
            Err(err) => Err(err),
        }
    }

    async fn exec_in(
        &self,
        handle: &RemoteSandboxHandle,
        command: &str,
        _cwd: Option<&str>,
        timeout_seconds: u64,
    ) -> SandboxResult<RemoteExecOutput> {
        let base = resolve_connector_base_url(
            &self.options,
            handle.extra.get("pod_ip").map(String::as_str),
        )?;
        let url = join_url(&base, "execute");
        let body = build_execute_body(command);
        let client = self.client.clone();
        let retry = self.retry.clone();
        let _ = timeout_seconds;
        let value = with_retries(&retry, move || {
            let client = client.clone();
            let url = url.clone();
            let body = body.clone();
            async move {
                let response = client
                    .post(&url)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
                Self::decode_json(response, &url).await
            }
        })
        .await?;
        let parsed = parse_execute_result(&value);
        Ok(RemoteExecOutput {
            exit_code: parsed.exit_code,
            stdout: parsed.stdout,
            stderr: parsed.stderr,
            truncated: false,
        })
    }
}

/// Kubernetes sandbox client (harness [`agentscope_harness::agent::sandbox::SandboxClient`]).
pub type KubernetesSandboxClient = RemoteSandboxClient;

/// Creates the harness [`agentscope_harness::agent::sandbox::SandboxClient`] for Kubernetes.
pub fn kubernetes_client(
    options: KubernetesSandboxClientOptions,
) -> SandboxResult<KubernetesSandboxClient> {
    Ok(RemoteSandboxClient::new(Arc::new(KubernetesBackend::new(options)?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_body_shape() {
        let body = build_claim_body("ns", "agentscope-s1", "pool-a");
        assert_eq!(body["apiVersion"], "extensions.agents.x-k8s.io/v1beta1");
        assert_eq!(body["kind"], "SandboxClaim");
        assert_eq!(body["metadata"]["name"], "agentscope-s1");
        assert_eq!(body["metadata"]["namespace"], "ns");
        assert_eq!(body["metadata"]["labels"][CREATED_BY_LABEL], CREATED_BY_VALUE);
        assert_eq!(body["spec"]["warmPoolRef"]["name"], "pool-a");
    }

    #[test]
    fn execute_body_shape() {
        assert_eq!(build_execute_body("ls -la"), json!({"command": "ls -la"}));
    }

    #[test]
    fn parse_execute_result_reads_fields() {
        let parsed = parse_execute_result(&json!({"stdout": "o", "stderr": "e", "exitCode": 2}));
        assert_eq!(parsed.stdout, "o");
        assert_eq!(parsed.stderr, "e");
        assert_eq!(parsed.exit_code, 2);
        assert_eq!(parse_execute_result(&json!({})).exit_code, -1);
    }

    #[test]
    fn parse_claim_status_detects_ready_and_pod() {
        let value = json!({
            "status": {
                "sandbox": {"name": "sandbox-1", "podIPs": ["10.0.0.1"]},
                "conditions": [
                    {"type": "Available", "status": "True"},
                    {"type": "Ready", "status": "True"}
                ]
            }
        });
        let status = parse_claim_status(&value);
        assert!(status.ready);
        assert_eq!(status.sandbox_name.as_deref(), Some("sandbox-1"));
        assert_eq!(status.pod_ips, vec!["10.0.0.1"]);
    }

    #[test]
    fn parse_claim_status_surfaces_failure_reason() {
        let value = json!({
            "status": {
                "conditions": [
                    {"type": "Ready", "status": "False", "reason": "Pending", "message": "waiting"}
                ]
            }
        });
        let status = parse_claim_status(&value);
        assert!(!status.ready);
        assert_eq!(status.reason.as_deref(), Some("waiting"));
    }

    #[test]
    fn connector_url_prefers_pod_ip_then_gateway() {
        let options = KubernetesSandboxClientOptions {
            gateway_name: Some("gw".to_string()),
            gateway_namespace: Some("ns".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_connector_base_url(&options, Some("10.0.0.5")).unwrap(),
            "http://10.0.0.5:8888"
        );
        assert_eq!(
            resolve_connector_base_url(&options, None).unwrap(),
            "http://gw.ns.svc.cluster.local:8888"
        );
        let bare = KubernetesSandboxClientOptions::default();
        assert!(resolve_connector_base_url(&bare, None).is_err());
    }

    #[test]
    fn claim_path_uses_crd_plural() {
        let backend = KubernetesBackend::new(KubernetesSandboxClientOptions {
            namespace: "team-a".to_string(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            backend.claims_collection_path(),
            "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims"
        );
        assert_eq!(
            backend.claim_path("agentscope-s1"),
            "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims/agentscope-s1"
        );
    }

    #[tokio::test]
    async fn create_requires_warm_pool_and_api_url() {
        let backend = KubernetesBackend::new(KubernetesSandboxClientOptions::default()).unwrap();
        assert!(matches!(
            backend.create_sandbox("s").await.unwrap_err(),
            SandboxExtensionError::Configuration(_)
        ));
    }

    #[tokio::test]
    async fn exec_requires_reachable_endpoint() {
        let backend = KubernetesBackend::new(KubernetesSandboxClientOptions::default()).unwrap();
        let handle = RemoteSandboxHandle::new("claim", BTreeMap::new());
        assert!(matches!(
            backend.exec_in(&handle, "ls", None, 5).await.unwrap_err(),
            SandboxExtensionError::Configuration(_)
        ));
    }
}
