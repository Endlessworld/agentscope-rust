//! Kubernetes (agent-sandbox CRD) sandbox backend.
//!
//! Mirrors the Java `io.agentscope.extensions.sandbox.kubernetes` package: sandbox provisioning
//! through the `extensions.agents.x-k8s.io/v1beta1` `SandboxClaim` CRD, plus command execution
//! against the sandbox server's `POST /execute` endpoint.
//!
//! The Java client supports four connection strategies (direct / gateway / in-cluster /
//! port-forward) and Rust implements all four — [`KubernetesConnectionStrategy`] `Direct` (a
//! pre-configured router URL), `InCluster` (pod IP, then the per-sandbox service DNS name),
//! `Gateway` (the external address published on the Gateway CRD, or an injected address) and
//! `PortForward` (a local tunnel to the router pod, resolved through the injected
//! [`KubernetesPortForwarder`] seam — Rust has no fabric8 `LocalPortForward`). Router-based
//! strategies inject the `X-Sandbox-ID` / `X-Sandbox-Namespace` / `X-Sandbox-Port` /
//! `X-Sandbox-Pod-IP` routing headers exactly like the Java `SandboxConnector`.
//!
//! When [`KubernetesSandboxClientOptions::file_api_base_dir`] is set, workspace archives and
//! single-file transfers go through the sandbox runtime file API
//! ([`crate::kubernetes_files`]) instead of base64-over-exec, mirroring the Java
//! `KubernetesSandbox#persistViaFileApi` / `SandboxFileTransfer` paths.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use reqwest::{Client, Response};
use serde_json::{json, Value};

use crate::error::{SandboxErrorCode, SandboxExtensionError, SandboxResult};
use crate::http::{bypass_proxy_for_loopback, join_url};
use crate::kubernetes_files::{
    file_api_path, temp_archive_rel_path, ExecTransport, KubernetesFileApi, KubernetesFileTransfer,
    ReqwestFileApiTransport, SandboxFileTransfer, FILE_API_TMP_DIR_REL,
};
use crate::remote::{
    ensure_output_success, hydrate_tar, persist_tar, RemoteBackend, RemoteExecOutput,
    RemoteSandboxClient, RemoteSandboxHandle, TAR_TIMEOUT_SECONDS,
};
use crate::retry::{with_retries, RetryPolicy};
use crate::workspace::shell_quote;

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
/// Gateway API group (mirrors Java `Constants.GATEWAY_API_GROUP`).
pub const GATEWAY_API_GROUP: &str = "gateway.networking.k8s.io";
/// Gateway API version (mirrors Java `Constants.GATEWAY_API_VERSION`).
pub const GATEWAY_API_VERSION: &str = "v1";
/// Gateway API plural name (mirrors Java `Constants.GATEWAY_PLURAL`).
pub const GATEWAY_PLURAL: &str = "gateways";
/// Router service fronting every sandbox (mirrors Java `Constants.ROUTER_SERVICE_NAME`).
pub const ROUTER_SERVICE_NAME: &str = "sandbox-router-svc";
/// Port served by the router service (mirrors Java `Constants.ROUTER_PORT`).
pub const ROUTER_PORT: u16 = 8080;
/// Namespace the router lives in by default (mirrors Java `LocalTunnelConnectionConfig`).
pub const DEFAULT_ROUTER_NAMESPACE: &str = "agent-sandbox-system";

/// Router header carrying the sandbox (claim) id.
pub const HEADER_SANDBOX_ID: &str = "X-Sandbox-ID";
/// Router header carrying the sandbox namespace.
pub const HEADER_SANDBOX_NAMESPACE: &str = "X-Sandbox-Namespace";
/// Router header carrying the sandbox server port.
pub const HEADER_SANDBOX_PORT: &str = "X-Sandbox-Port";
/// Router header carrying the sandbox pod IP.
pub const HEADER_SANDBOX_POD_IP: &str = "X-Sandbox-Pod-IP";

/// How the sandbox HTTP endpoint is discovered (mirrors the Java `ConnectionStrategy` tree).
///
/// Java has four strategies (`DirectStrategy`, `GatewayStrategy`, `InClusterStrategy`,
/// `PortForwardStrategy`); Rust implements all four. Port-forwarding is a seam rather than a
/// fabric8 client: [`KubernetesPortForwarder`] supplies the local endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KubernetesConnectionStrategy {
    /// Use the pre-configured router API URL ([`KubernetesSandboxClientOptions::sandbox_api_url`]).
    ///
    /// Mirrors the Java `DirectStrategy` + `DirectConnectionConfig`.
    Direct,
    /// Reach the sandbox pod directly: its pod IP when known, the per-sandbox service DNS name
    /// otherwise.
    ///
    /// Mirrors the Java `InClusterStrategy` + `InClusterConnectionConfig`.
    InCluster,
    /// Route through the sandbox router behind a Kubernetes Gateway.
    ///
    /// Mirrors the Java `GatewayStrategy` + `GatewayConnectionConfig`: the external address is
    /// read from the Gateway CRD (`status.addresses[0].value`) or supplied by the caller via
    /// [`KubernetesSandboxClientOptions::gateway_address`] when the CRD is not readable.
    Gateway,
    /// Tunnel to the router pod through a local port.
    ///
    /// Mirrors the Java `PortForwardStrategy` + `LocalTunnelConnectionConfig`: the port forward
    /// targets the `sandbox-router-svc` pod on [`ROUTER_PORT`] and the connector base URL becomes
    /// `http://<local host>:<local port>`. The tunnel itself is not a fabric8 client in Rust but
    /// the injected [`KubernetesPortForwarder`]; selecting this strategy without a forwarder is a
    /// configuration error (never a silent fallback to another strategy).
    PortForward,
}

impl KubernetesConnectionStrategy {
    /// Stable string discriminator used in diagnostics and state metadata.
    pub fn as_str(self) -> &'static str {
        match self {
            KubernetesConnectionStrategy::Direct => "direct",
            KubernetesConnectionStrategy::InCluster => "in-cluster",
            KubernetesConnectionStrategy::Gateway => "gateway",
            KubernetesConnectionStrategy::PortForward => "port-forward",
        }
    }

    /// Whether `X-Sandbox-*` router headers must be injected (Java
    /// `ConnectionStrategy#shouldInjectRouterHeaders`, `false` only for in-cluster).
    ///
    /// `PortForward` still talks to the router service (through the tunnel), so it injects them
    /// exactly like `Direct`.
    pub fn should_inject_router_headers(self) -> bool {
        !matches!(self, KubernetesConnectionStrategy::InCluster)
    }
}

/// Local endpoint produced by a [`KubernetesPortForwarder`] (Java
/// `PortForwardStrategy#connect` returns `http://127.0.0.1:<localPort>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardedEndpoint {
    /// Local host to dial (normally `127.0.0.1`).
    pub local_host: String,
    /// Local port bound by the tunnel (fabric8 `LocalPortForward#getLocalPort`).
    pub local_port: u16,
}

impl ForwardedEndpoint {
    /// Creates an endpoint from a host and port.
    pub fn new(local_host: impl Into<String>, local_port: u16) -> Self {
        Self {
            local_host: local_host.into(),
            local_port,
        }
    }

    /// Renders `scheme://host:port`, bracketing IPv6 literals like the other strategies.
    pub fn base_url(&self, scheme: &str) -> String {
        format!(
            "{}://{}:{}",
            scheme.trim(),
            host_for_url(self.local_host.trim()),
            self.local_port
        )
    }
}

/// Minimal seam for the Java `PortForwardStrategy` (fabric8 `LocalPortForward`).
///
/// Java opens a fabric8 port-forward from `localhost:<localPort>` to the sandbox router pod and
/// derives the connector base URL from it. Rust has no fabric8 client, so the tunnel is injected:
/// implementations own whatever transport they have (a kube-rs port-forward, an out-of-process
/// `kubectl port-forward` child, a test double) and report the local endpoint back.
///
/// Implementations should be **idempotent** for a given
/// `(namespace, pod_name, remote_port)`: [`KubernetesBackend::connector_base_url`] may ask for the
/// same tunnel more than once per sandbox and expects the same live endpoint back (Java caches the
/// forward while `LocalPortForward#isAlive()`).
#[async_trait::async_trait]
pub trait KubernetesPortForwarder: std::fmt::Debug + Send + Sync {
    /// Opens (or reuses) a tunnel from a local port to `remote_port` on `pod_name`.
    async fn forward(
        &self,
        sandbox_id: &str,
        namespace: &str,
        pod_name: &str,
        remote_port: u16,
    ) -> SandboxResult<ForwardedEndpoint>;
}

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
    ///
    /// Workspace archives and single-file transfers go through the runtime file API when this is
    /// non-blank; set it to blank to fall back to base64-over-exec. Defaults to
    /// [`DEFAULT_WORKSPACE_ROOT`], matching the Java `KubernetesSandboxClientOptions`.
    pub file_api_base_dir: String,
    /// Kubernetes API server URL (e.g. `https://kubernetes.default.svc`).
    pub api_url: Option<String>,
    /// Pre-configured sandbox router API URL used by the `Direct` strategy.
    ///
    /// Mirrors the Java `KubernetesSandboxClientOptions#apiUrl` (consumed by `DirectStrategy`).
    /// Distinct from [`Self::api_url`], which addresses the Kubernetes API server itself.
    pub sandbox_api_url: Option<String>,
    /// Explicit connection strategy; when `None` it is derived from the configured fields with
    /// the Java priority (direct URL, then gateway, then in-cluster).
    pub connection_strategy: Option<KubernetesConnectionStrategy>,
    /// Injected Gateway external address (host or host:port).
    ///
    /// Used by the `Gateway` strategy when the Gateway CRD cannot be read (no RBAC / no
    /// `api_url`); the Java `GatewayStrategy` always reads the CRD.
    pub gateway_address: Option<String>,
    /// Injected port-forwarder used by the `PortForward` strategy.
    ///
    /// Mirrors the Java `PortForwardStrategy` (fabric8 `LocalPortForward`). Rust has no fabric8
    /// client, so the tunnel is a seam: selecting `PortForward` without a forwarder is a
    /// configuration error and never falls back to another strategy.
    pub port_forwarder: Option<Arc<dyn KubernetesPortForwarder>>,
    /// Namespace of the router service targeted by the port-forward (Java
    /// `LocalTunnelConnectionConfig.routerNamespace`, default [`DEFAULT_ROUTER_NAMESPACE`]).
    pub port_forward_router_namespace: String,
    /// Router pod to forward to.
    ///
    /// When `None` the pod name is read from the router service `Endpoints`
    /// (`subsets[].addresses[].targetRef.name`, Java `PortForwardStrategy#resolveRouterPod`).
    /// Set it explicitly when the `Endpoints` resource is not readable.
    pub port_forward_pod: Option<String>,
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
            sandbox_api_url: None,
            connection_strategy: None,
            gateway_address: None,
            port_forwarder: None,
            port_forward_router_namespace: DEFAULT_ROUTER_NAMESPACE.to_string(),
            port_forward_pod: None,
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

impl KubernetesSandboxClientOptions {
    /// Resolves the effective connection strategy.
    ///
    /// An explicit [`Self::connection_strategy`] wins; otherwise the Java priority order is
    /// reproduced: a pre-configured router URL (`Direct`), then a gateway name (`Gateway`), then
    /// in-cluster. Java falls back to `LocalTunnelConnectionConfig` (port-forward) for the last
    /// case; Rust keeps `InCluster` as the default because the tunnel would otherwise require an
    /// injected [`KubernetesPortForwarder`] — `PortForward` is therefore only selected explicitly
    /// (tracked difference, see the module tracker).
    pub fn resolve_connection_strategy(&self) -> KubernetesConnectionStrategy {
        if let Some(strategy) = self.connection_strategy {
            return strategy;
        }
        if self
            .sandbox_api_url
            .as_deref()
            .map(|u| !u.trim().is_empty())
            .unwrap_or(false)
        {
            return KubernetesConnectionStrategy::Direct;
        }
        if self
            .gateway_name
            .as_deref()
            .map(|g| !g.trim().is_empty())
            .unwrap_or(false)
        {
            return KubernetesConnectionStrategy::Gateway;
        }
        KubernetesConnectionStrategy::InCluster
    }

    /// Gateway namespace, falling back to the sandbox namespace (Java
    /// `KubernetesSandboxClient.toConnectionConfig`).
    pub fn effective_gateway_namespace(&self) -> String {
        self.gateway_namespace
            .clone()
            .filter(|ns| !ns.trim().is_empty())
            .unwrap_or_else(|| self.namespace.clone())
    }

    /// Whether the runtime file API is enabled (Java `fileApiEnabled`).
    pub fn file_api_enabled(&self) -> bool {
        !self.file_api_base_dir.trim().is_empty()
    }
}

/// Builds the `SandboxClaim` CRD body for a deterministic claim name.
pub fn build_claim_body(namespace: &str, claim_name: &str, warm_pool_name: &str) -> Value {
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
            let cstatus = condition
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("");
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
    KubernetesClaimStatus {
        sandbox_name,
        pod_ips,
        ready,
        reason,
    }
}

/// Extracts the claim names from a Kubernetes `SandboxClaimList` payload.
///
/// Mirrors the Java `K8sHelper#listSandboxClaims`: the server order is preserved and claims
/// without `metadata.name` are skipped (blank names are skipped too, so the result is directly
/// usable for follow-up `DELETE` calls).
pub fn parse_claim_names(value: &Value) -> Vec<String> {
    value
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.get("metadata")
                        .and_then(|metadata| metadata.get("name"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Returns `spec.warmPoolRef.name` from a claim payload.
///
/// Mirrors the Java `SandboxClient#getSandboxClaimWarmPoolName`, which returns `null` when the
/// claim carries no `spec` or no `warmPoolRef`.
pub fn claim_warm_pool_name(claim: &Value) -> Option<String> {
    claim
        .get("spec")
        .and_then(|spec| spec.get("warmPoolRef"))
        .and_then(|reference| reference.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// Polls `fetch` until it yields an address or `budget` elapses.
///
/// Returns `Ok(None)` on timeout and propagates fetch errors unchanged. Extracted from
/// [`KubernetesBackend::wait_for_gateway_address`] so the wait/cancel shape is testable without
/// sleeping or touching the network.
async fn wait_for_gateway_address_with<F, Fut>(
    budget: std::time::Duration,
    interval: std::time::Duration,
    mut fetch: F,
) -> SandboxResult<Option<String>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = SandboxResult<Option<String>>>,
{
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(address) = fetch().await? {
            return Ok(Some(address));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(interval.max(std::time::Duration::from_millis(1))).await;
    }
}

/// Resolves the sandbox server base URL for an explicit connection strategy.
///
/// Mirrors the Java strategy tree:
///
/// * [`KubernetesConnectionStrategy::Direct`] — the pre-configured router URL
///   ([`KubernetesSandboxClientOptions::sandbox_api_url`]).
/// * [`KubernetesConnectionStrategy::InCluster`] — `http://<podIp>:<port>` when a pod IP is
///   known, otherwise the per-sandbox service DNS name
///   `http://<sandboxId>.<namespace>.svc.cluster.local:<port>` (Java `InClusterStrategy`).
/// * [`KubernetesConnectionStrategy::Gateway`] — `scheme://<gatewayAddress>` from the Gateway
///   CRD status address (Java `GatewayStrategy#formatUrl`; the port is **not** appended, IPv6
///   literals are bracketed).
/// * [`KubernetesConnectionStrategy::PortForward`] — not resolvable here: the tunnel is async and
///   needs the injected forwarder, so use [`resolve_port_forward_base_url`] (or
///   [`KubernetesBackend::connector_base_url`]). Passing it to this function is a configuration
///   error, never a silent fallback to in-cluster.
///
/// `sandbox_id` is the sandbox resource name when available (`status.sandbox.name`) and the
/// claim name otherwise.
pub fn resolve_connector_base_url(
    options: &KubernetesSandboxClientOptions,
    strategy: KubernetesConnectionStrategy,
    sandbox_id: &str,
    pod_ip: Option<&str>,
    gateway_address: Option<&str>,
) -> SandboxResult<String> {
    match strategy {
        KubernetesConnectionStrategy::Direct => {
            let url = options
                .sandbox_api_url
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .ok_or_else(|| {
                    SandboxExtensionError::Configuration(
                        "Kubernetes Direct strategy needs a router API URL \
                         (KubernetesSandboxClientOptions::sandbox_api_url)"
                            .to_string(),
                    )
                })?;
            Ok(url.trim_end_matches('/').to_string())
        }
        KubernetesConnectionStrategy::InCluster => {
            if let Some(pod_ip) = pod_ip.filter(|ip| !ip.trim().is_empty()) {
                return Ok(format!(
                    "http://{}:{}",
                    host_for_url(pod_ip),
                    options.server_port
                ));
            }
            let id = sandbox_id.trim();
            if id.is_empty() {
                return Err(SandboxExtensionError::Configuration(
                    "Kubernetes in-cluster strategy needs a sandbox id for the service DNS name"
                        .to_string(),
                ));
            }
            Ok(format!(
                "http://{id}.{}.svc.cluster.local:{}",
                options.namespace, options.server_port
            ))
        }
        KubernetesConnectionStrategy::Gateway => {
            let address = gateway_address
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .ok_or_else(|| {
                    SandboxExtensionError::Configuration(format!(
                        "Kubernetes Gateway strategy could not resolve a gateway address \
                         (CRD status.addresses[0].value or KubernetesSandboxClientOptions::\
                         gateway_address); gateway={:?} namespace={}",
                        options.gateway_name,
                        options.effective_gateway_namespace()
                    ))
                })?;
            Ok(format!(
                "{}://{}",
                options.gateway_scheme.trim(),
                host_for_url(address)
            ))
        }
        KubernetesConnectionStrategy::PortForward => Err(SandboxExtensionError::Configuration(
            "Kubernetes PortForward strategy needs the async port-forward seam; call \
             resolve_port_forward_base_url (or KubernetesBackend::connector_base_url) instead of \
             resolve_connector_base_url"
                .to_string(),
        )),
    }
}

/// Resolves the connector base URL for [`KubernetesConnectionStrategy::PortForward`].
///
/// Mirrors the Java `PortForwardStrategy#connect`: the router pod is port-forwarded on
/// [`ROUTER_PORT`] and the base URL becomes the local endpoint (`http://127.0.0.1:<port>`).
/// Unlike Java there is no implicit fabric8 client, so a missing
/// [`KubernetesSandboxClientOptions::port_forwarder`] is reported as a configuration error — the
/// caller never silently gets another strategy's URL.
pub async fn resolve_port_forward_base_url(
    options: &KubernetesSandboxClientOptions,
    sandbox_id: &str,
    pod_name: &str,
) -> SandboxResult<String> {
    let forwarder = options.port_forwarder.as_ref().ok_or_else(|| {
        SandboxExtensionError::Configuration(
            "Kubernetes PortForward strategy requires an injected port forwarder \
             (KubernetesSandboxClientOptions::port_forwarder); refusing to fall back to another \
             strategy"
                .to_string(),
        )
    })?;
    let pod = pod_name.trim();
    if pod.is_empty() {
        return Err(SandboxExtensionError::Configuration(
            "Kubernetes PortForward strategy needs the router pod name \
             (KubernetesSandboxClientOptions::port_forward_pod or the sandbox-router-svc Endpoints)"
                .to_string(),
        ));
    }
    let endpoint = forwarder
        .forward(
            sandbox_id,
            &options.port_forward_router_namespace,
            pod,
            ROUTER_PORT,
        )
        .await?;
    Ok(endpoint.base_url("http"))
}

/// Renders a host for URL interpolation, bracketing IPv6 literals (Java
/// `InClusterStrategy` / `GatewayStrategy` both do this).
fn host_for_url(host: &str) -> String {
    let trimmed = host.trim();
    if trimmed.contains(':') && !trimmed.starts_with('[') {
        format!("[{trimmed}]")
    } else {
        trimmed.to_string()
    }
}

/// Extracts the first external address from a Gateway CRD (`status.addresses[0].value`).
pub fn parse_gateway_address(value: &Value) -> Option<String> {
    value
        .get("status")
        .and_then(|status| status.get("addresses"))
        .and_then(Value::as_array)
        .and_then(|addresses| addresses.first())
        .and_then(|address| address.get("value"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
}

/// Extracts the first ready router pod name from an `Endpoints` resource
/// (`subsets[].addresses[].targetRef.name`).
///
/// Mirrors the Java `PortForwardStrategy#resolveRouterPod`: subsets without addresses are
/// skipped, and addresses without a `targetRef.name` are ignored.
pub fn parse_endpoint_target_pod(value: &Value) -> Option<String> {
    let subsets = value.get("subsets")?.as_array()?;
    for subset in subsets {
        let Some(addresses) = subset.get("addresses").and_then(Value::as_array) else {
            continue;
        };
        for address in addresses {
            let name = address
                .get("targetRef")
                .and_then(|target| target.get("name"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty());
            if let Some(name) = name {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Builds the `X-Sandbox-*` router headers injected for router-based strategies.
///
/// Mirrors the Java `SandboxConnector#buildRequest`: the pod IP header is only present when the
/// pod IP is known.
pub fn connector_router_headers(
    sandbox_id: &str,
    namespace: &str,
    server_port: u16,
    pod_ip: Option<&str>,
) -> Vec<(String, String)> {
    let mut headers = vec![
        (HEADER_SANDBOX_ID.to_string(), sandbox_id.to_string()),
        (HEADER_SANDBOX_NAMESPACE.to_string(), namespace.to_string()),
        (HEADER_SANDBOX_PORT.to_string(), server_port.to_string()),
    ];
    if let Some(pod_ip) = pod_ip.map(str::trim).filter(|ip| !ip.is_empty()) {
        headers.push((HEADER_SANDBOX_POD_IP.to_string(), pod_ip.to_string()));
    }
    headers
}

/// Kubernetes [`RemoteBackend`] implementation.
///
/// Cheap to clone (the `reqwest` client and retry policy are reference-counted) so the native
/// file-transfer adapter can own a snapshot of the backend together with the live handle.
#[derive(Debug, Clone)]
pub struct KubernetesBackend {
    options: KubernetesSandboxClientOptions,
    client: Client,
    retry: RetryPolicy,
    /// Claims this backend created or attached to, as `"{namespace}/{claimName}"` keys.
    ///
    /// Mirrors the Java `SandboxClient#activeSandboxes` registry: the Java map is keyed the same
    /// way and drives `listActiveSandboxes()` / `deleteAll()` / `close()`. Java prunes entries
    /// whose client-side `Sandbox` is no longer active; a claim-only backend has no such liveness
    /// signal, so entries are dropped by [`KubernetesBackend::kill_sandbox`],
    /// [`KubernetesBackend::delete_sandbox_claim`] and [`KubernetesBackend::delete_all`].
    active_claims: Arc<Mutex<BTreeSet<String>>>,
}

impl KubernetesBackend {
    /// Builds a backend, wiring up the reqwest client and retry policy.
    pub fn new(options: KubernetesSandboxClientOptions) -> SandboxResult<Self> {
        // Any of the configured endpoints can be a locally served router (port-forward, local
        // gateway); such traffic must stay off the ambient proxy.
        let client = bypass_proxy_for_loopback(
            Client::builder()
                .connect_timeout(std::time::Duration::from_secs(
                    options.connect_timeout_seconds,
                ))
                .timeout(std::time::Duration::from_secs(options.read_timeout_seconds)),
            &[
                options.api_url.as_deref().unwrap_or_default(),
                options.sandbox_api_url.as_deref().unwrap_or_default(),
                options.gateway_address.as_deref().unwrap_or_default(),
            ],
        )
        .build()
        .map_err(|e| SandboxExtensionError::Transport(format!("failed to build client: {e}")))?;
        let retry = RetryPolicy::kubernetes(options.max_retries);
        Ok(Self {
            options,
            client,
            retry,
            active_claims: Arc::new(Mutex::new(BTreeSet::new())),
        })
    }

    /// Returns the configured options.
    pub fn options(&self) -> &KubernetesSandboxClientOptions {
        &self.options
    }

    fn claims_collection_path(&self) -> String {
        self.claims_collection_path_in(&self.options.namespace)
    }

    fn claims_collection_path_in(&self, namespace: &str) -> String {
        format!(
            "/apis/{CLAIM_API_GROUP}/{CLAIM_API_VERSION}/namespaces/{namespace}/{CLAIM_PLURAL_NAME}"
        )
    }

    fn claim_path(&self, claim_name: &str) -> String {
        self.claim_path_in(&self.options.namespace, claim_name)
    }

    fn claim_path_in(&self, namespace: &str, claim_name: &str) -> String {
        format!("{}/{claim_name}", self.claims_collection_path_in(namespace))
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

    /// Effective connection strategy (explicit option or Java-priority derivation).
    pub fn connection_strategy(&self) -> KubernetesConnectionStrategy {
        self.options.resolve_connection_strategy()
    }

    // ------------------------------------------------------- claim registry / listing (K8sHelper)

    fn claim_key(&self, namespace: &str, claim_name: &str) -> String {
        format!("{namespace}/{claim_name}")
    }

    fn active_claims_guard(&self) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
        self.active_claims
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn track_claim(&self, claim_name: &str) {
        let key = self.claim_key(&self.options.namespace, claim_name);
        self.active_claims_guard().insert(key);
    }

    fn untrack_claim(&self, namespace: &str, claim_name: &str) {
        self.active_claims_guard()
            .remove(&self.claim_key(namespace, claim_name));
    }

    /// Returns the `"{namespace}/{claimName}"` keys this backend created or attached to.
    ///
    /// Mirrors the Java `SandboxClient#listActiveSandboxes` (which returns the `activeSandboxes`
    /// key set after pruning sandboxes whose client-side handle is no longer active). The set is
    /// sorted so the result is deterministic; see the `active_claims` field for the liveness
    /// difference.
    pub fn list_active_sandboxes(&self) -> Vec<String> {
        self.active_claims_guard().iter().cloned().collect()
    }

    /// Lists `SandboxClaim` names in the configured namespace, optionally filtered by a label
    /// selector (`?labelSelector=`), mirroring the Java `K8sHelper#listSandboxClaims`.
    ///
    /// Names keep the server's ordering and claims without `metadata.name` are skipped, exactly
    /// like the Java implementation.
    pub async fn list_sandbox_claims(
        &self,
        label_selector: Option<&str>,
    ) -> SandboxResult<Vec<String>> {
        let path = self.claims_collection_path();
        let selector = label_selector.map(str::trim).filter(|s| !s.is_empty());
        let query: Vec<(&str, &str)> = match selector {
            Some(selector) => vec![("labelSelector", selector)],
            None => Vec::new(),
        };
        let value = self
            .k8s_request_query(reqwest::Method::GET, &path, &query, None)
            .await?;
        Ok(parse_claim_names(&value))
    }

    /// Fetches a single claim, returning `Ok(None)` for HTTP 404 (the Java
    /// `K8sHelper#getSandboxClaim` returns `null`).
    pub async fn get_sandbox_claim(&self, claim_name: &str) -> SandboxResult<Option<Value>> {
        let path = self.claim_path(claim_name);
        match self.k8s_request(reqwest::Method::GET, &path, None).await {
            Ok(value) => Ok(Some(value)),
            Err(SandboxExtensionError::Http { status: 404, .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Returns the warm-pool name referenced by a claim.
    ///
    /// Mirrors the Java `SandboxClient#getSandboxClaimWarmPoolName`: a missing claim is an error,
    /// and a claim without `spec.warmPoolRef` yields `Ok(None)`.
    pub async fn get_sandbox_claim_warm_pool_name(
        &self,
        claim_name: &str,
    ) -> SandboxResult<Option<String>> {
        let claim = self.get_sandbox_claim(claim_name).await?;
        let Some(claim) = claim else {
            return Err(SandboxExtensionError::Runtime(
                SandboxErrorCode::WorkspaceStartError,
                format!(
                    "SandboxClaim '{claim_name}' not found in namespace '{}'.",
                    self.options.namespace
                ),
            ));
        };
        Ok(claim_warm_pool_name(&claim))
    }

    /// Deletes a claim, tolerating HTTP 404 and blank names, mirroring the Java
    /// `K8sHelper#deleteSandboxClaim` ("no-op if already deleted").
    pub async fn delete_sandbox_claim(&self, claim_name: &str) -> SandboxResult<()> {
        self.delete_claim_in(&self.options.namespace, claim_name)
            .await
    }

    async fn delete_claim_in(&self, namespace: &str, claim_name: &str) -> SandboxResult<()> {
        if claim_name.trim().is_empty() {
            return Ok(());
        }
        let path = self.claim_path_in(namespace, claim_name);
        let result = match self.k8s_request(reqwest::Method::DELETE, &path, None).await {
            Ok(_) => Ok(()),
            Err(SandboxExtensionError::Http { status: 404, .. }) => Ok(()),
            Err(err) => Err(err),
        };
        self.untrack_claim(namespace, claim_name);
        result
    }

    /// Deletes every claim this backend tracks, mirroring the Java `SandboxClient#deleteAll`:
    /// all tracked claims are attempted, the registry is cleared, and the first failure is
    /// returned after the remaining deletions have been tried.
    pub async fn delete_all(&self) -> SandboxResult<()> {
        let keys: Vec<String> = self.list_active_sandboxes();
        let mut first_error: Option<SandboxExtensionError> = None;
        for key in keys {
            let Some((namespace, claim_name)) = key.split_once('/') else {
                continue;
            };
            if let Err(err) = self.delete_claim_in(namespace, claim_name).await {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
        self.active_claims_guard().clear();
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// The sandbox resource name when known, the claim name otherwise (used by the in-cluster
    /// service DNS name).
    pub fn sandbox_id_of(handle: &RemoteSandboxHandle) -> String {
        handle
            .extra
            .get("sandbox_name")
            .map(String::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(&handle.id)
            .to_string()
    }

    /// Resolves the Gateway external address, waiting up to the sandbox-ready budget for the
    /// Gateway CRD to publish one.
    ///
    /// Mirrors the Java `GatewayStrategy#connect`, which reads the address once and then
    /// **watches** the Gateway for `gatewayReadyTimeoutSeconds` until an address appears (a
    /// deleted Gateway is an error). Rust polls [`Self::resolve_gateway_address`] on the same
    /// interval as the claim readiness wait and gives up with a configuration error; no
    /// background task is spawned, so there is nothing to cancel or leak. Continuous
    /// change-notification is not needed: unlike Java (which caches the formatted base URL until
    /// `close()`), [`Self::connector_base_url`] re-resolves on every call, so an address change is
    /// picked up by the next request.
    pub async fn wait_for_gateway_address(&self) -> SandboxResult<String> {
        let budget =
            std::time::Duration::from_secs(self.options.sandbox_ready_timeout_seconds.max(1));
        let interval = std::time::Duration::from_millis(READY_POLL_INTERVAL_MS);
        match wait_for_gateway_address_with(budget, interval, || self.resolve_gateway_address())
            .await?
        {
            Some(address) => Ok(address),
            None => Err(SandboxExtensionError::Runtime(
                SandboxErrorCode::ConfigurationError,
                format!(
                    "Timed out waiting for gateway address: {}",
                    self.options.gateway_name.as_deref().unwrap_or("")
                ),
            )),
        }
    }

    /// Resolves the Gateway external address.
    ///
    /// An injected [`KubernetesSandboxClientOptions::gateway_address`] wins (the escape hatch for
    /// clusters where the Gateway CRD is not readable); otherwise the address is read from
    /// `GET /apis/gateway.networking.k8s.io/v1/namespaces/{ns}/gateways/{name}` →
    /// `status.addresses[0].value`, mirroring the Java `GatewayStrategy#resolveGatewayAddress`.
    /// A failed read is reported as `Ok(None)` so the caller raises a configuration error instead
    /// of a transport error.
    pub async fn resolve_gateway_address(&self) -> SandboxResult<Option<String>> {
        if let Some(address) = self
            .options
            .gateway_address
            .as_deref()
            .map(str::trim)
            .filter(|address| !address.is_empty())
        {
            return Ok(Some(address.to_string()));
        }
        let Some(name) = self
            .options
            .gateway_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            return Ok(None);
        };
        if self
            .options
            .api_url
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            return Ok(None);
        }
        let path = format!(
            "/apis/{GATEWAY_API_GROUP}/{GATEWAY_API_VERSION}/namespaces/{}/{GATEWAY_PLURAL}/{name}",
            self.options.effective_gateway_namespace()
        );
        match self.k8s_request(reqwest::Method::GET, &path, None).await {
            Ok(value) => Ok(parse_gateway_address(&value)),
            Err(err) => {
                tracing::warn!("kubernetes: failed to read gateway {name}: {err}");
                Ok(None)
            }
        }
    }

    /// Resolves the router pod targeted by the `PortForward` strategy.
    ///
    /// An injected [`KubernetesSandboxClientOptions::port_forward_pod`] wins (the escape hatch for
    /// clusters where the `Endpoints` resource is not readable); otherwise the pod name is read
    /// from `GET /api/v1/namespaces/{ns}/endpoints/sandbox-router-svc` →
    /// `subsets[].addresses[].targetRef.name`, mirroring the Java
    /// `PortForwardStrategy#resolveRouterPod`. A failed read is reported as `Ok(None)` so the
    /// caller raises a configuration error instead of a transport error.
    pub async fn resolve_router_pod(&self) -> SandboxResult<Option<String>> {
        if let Some(pod) = self
            .options
            .port_forward_pod
            .as_deref()
            .map(str::trim)
            .filter(|pod| !pod.is_empty())
        {
            return Ok(Some(pod.to_string()));
        }
        if self
            .options
            .api_url
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            return Ok(None);
        }
        let path = format!(
            "/api/v1/namespaces/{}/endpoints/{ROUTER_SERVICE_NAME}",
            self.options.port_forward_router_namespace
        );
        match self.k8s_request(reqwest::Method::GET, &path, None).await {
            Ok(value) => Ok(parse_endpoint_target_pod(&value)),
            Err(err) => {
                tracing::warn!("kubernetes: failed to read {ROUTER_SERVICE_NAME} endpoints: {err}");
                Ok(None)
            }
        }
    }

    /// Resolves the sandbox HTTP base URL for `handle` according to the effective strategy.
    ///
    /// `PortForward` is handled before the synchronous resolver: it needs the async tunnel and
    /// the router pod, and a missing forwarder/pod is a configuration error (never a silent
    /// fallback to `InCluster`).
    pub async fn connector_base_url(&self, handle: &RemoteSandboxHandle) -> SandboxResult<String> {
        let strategy = self.connection_strategy();
        let sandbox_id = Self::sandbox_id_of(handle);
        if strategy == KubernetesConnectionStrategy::PortForward {
            let pod = self.resolve_router_pod().await?.ok_or_else(|| {
                SandboxExtensionError::Configuration(format!(
                    "Kubernetes PortForward strategy could not resolve the router pod \
                     (KubernetesSandboxClientOptions::port_forward_pod or the {ROUTER_SERVICE_NAME} \
                     Endpoints in namespace {})",
                    self.options.port_forward_router_namespace
                ))
            })?;
            return resolve_port_forward_base_url(&self.options, &sandbox_id, &pod).await;
        }
        let gateway_address = if strategy == KubernetesConnectionStrategy::Gateway {
            Some(self.wait_for_gateway_address().await?)
        } else {
            None
        };
        resolve_connector_base_url(
            &self.options,
            strategy,
            &sandbox_id,
            handle.extra.get("pod_ip").map(String::as_str),
            gateway_address.as_deref(),
        )
    }

    /// `X-Sandbox-*` router headers for `handle` (empty for the in-cluster strategy, exactly like
    /// the Java `ConnectionStrategy#shouldInjectRouterHeaders`).
    pub fn router_headers(&self, handle: &RemoteSandboxHandle) -> Vec<(String, String)> {
        if !self.connection_strategy().should_inject_router_headers() {
            return Vec::new();
        }
        connector_router_headers(
            Self::sandbox_id_of(handle).as_str(),
            &self.options.namespace,
            self.options.server_port,
            handle.extra.get("pod_ip").map(String::as_str),
        )
    }

    /// Native file-transfer adapter backed by the runtime file API, when enabled.
    ///
    /// Mirrors the Java `KubernetesSandbox implements SandboxFileTransfer`: the adapter exists
    /// only when [`KubernetesSandboxClientOptions::file_api_base_dir`] is non-blank, and it
    /// declines paths outside that base dir so the caller falls back to base64-over-exec.
    pub async fn native_file_transfer(
        &self,
        handle: &RemoteSandboxHandle,
    ) -> SandboxResult<Option<Arc<dyn SandboxFileTransfer>>> {
        if !self.options.file_api_enabled() {
            return Ok(None);
        }
        let base_url = self.connector_base_url(handle).await?;
        let api = KubernetesFileApi::new(
            base_url,
            Arc::new(ReqwestFileApiTransport::new(self.client.clone())),
        )
        .with_headers(self.router_headers(handle))
        .with_exec(Arc::new(KubernetesHandleExec {
            backend: self.clone(),
            handle: handle.clone(),
        }));
        Ok(Some(Arc::new(KubernetesFileTransfer::new(
            api,
            self.options.file_api_base_dir.clone(),
        ))))
    }

    /// Tars the workspace into a temp file under the file-API base dir and downloads it.
    ///
    /// Mirrors the Java `KubernetesSandbox#persistViaFileApi`: the archive is written by `tar`
    /// inside the sandbox (never returned over stdout), fetched through the file API and deleted
    /// afterwards. The temp dir is excluded so a base dir equal to the workspace root cannot
    /// archive the archive itself.
    async fn persist_via_file_api(
        &self,
        handle: &RemoteSandboxHandle,
        root: &str,
        transfer: &Arc<dyn SandboxFileTransfer>,
    ) -> SandboxResult<Vec<u8>> {
        let relative = temp_archive_rel_path("ws-persist", &handle.id);
        let absolute = file_api_path(&self.options.file_api_base_dir, &relative);
        let Some(tmp_dir) = crate::workspace::parent_dir(&absolute) else {
            return Err(SandboxExtensionError::Configuration(format!(
                "file API base dir yields no parent directory: {absolute}"
            )));
        };
        let command = format!(
            "mkdir -p {} && tar --exclude=./{FILE_API_TMP_DIR_REL} -cf {} -C {} .",
            shell_quote(&tmp_dir),
            shell_quote(&absolute),
            shell_quote(root)
        );
        let output = self
            .exec_in(handle, &command, None, TAR_TIMEOUT_SECONDS)
            .await?;
        let downloaded = match ensure_output_success("workspace archive", &output) {
            Ok(()) => transfer.download_file(&absolute).await,
            Err(err) => Err(err),
        };
        let _ = self
            .exec_in(
                handle,
                &format!("rm -f {}", shell_quote(&absolute)),
                None,
                30,
            )
            .await;
        downloaded
    }

    /// Uploads an archive through the file API and extracts it into `root`.
    ///
    /// Mirrors the Java `KubernetesSandbox#hydrateViaFileApi`.
    async fn hydrate_via_file_api(
        &self,
        handle: &RemoteSandboxHandle,
        root: &str,
        archive: &[u8],
        transfer: &Arc<dyn SandboxFileTransfer>,
    ) -> SandboxResult<()> {
        let relative = temp_archive_rel_path("ws-hydrate", &handle.id);
        let absolute = file_api_path(&self.options.file_api_base_dir, &relative);
        let Some(tmp_dir) = crate::workspace::parent_dir(&absolute) else {
            return Err(SandboxExtensionError::Configuration(format!(
                "file API base dir yields no parent directory: {absolute}"
            )));
        };
        let mkdir = self
            .exec_in(
                handle,
                &format!("mkdir -p {}", shell_quote(&tmp_dir)),
                None,
                30,
            )
            .await?;
        ensure_output_success("workspace hydrate staging", &mkdir)?;
        let extracted = match transfer.upload_file(&absolute, archive).await {
            Ok(()) => {
                let command = format!(
                    "tar -xf {} -C {}",
                    shell_quote(&absolute),
                    shell_quote(root)
                );
                match self
                    .exec_in(handle, &command, None, TAR_TIMEOUT_SECONDS)
                    .await
                {
                    Ok(output) => ensure_output_success("workspace hydrate extract", &output),
                    Err(err) => Err(err),
                }
            }
            Err(err) => Err(err),
        };
        let _ = self
            .exec_in(
                handle,
                &format!("rm -f {}", shell_quote(&absolute)),
                None,
                30,
            )
            .await;
        extracted
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
        self.k8s_request_query(method, path, &[], body).await
    }

    async fn k8s_request_query(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> SandboxResult<Value> {
        let base = self.api_base()?;
        let url = join_url(&base, path);
        let token = self.options.api_token.clone();
        let client = self.client.clone();
        let retry = self.retry.clone();
        let query: Vec<(String, String)> = query
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        with_retries(&retry, move || {
            let client = client.clone();
            let url = url.clone();
            let method = method.clone();
            let token = token.clone();
            let body = body.clone();
            let query = query.clone();
            async move {
                let mut request = client.request(method, &url);
                if !query.is_empty() {
                    request = request.query(&query);
                }
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

    fn handle_from_status(
        &self,
        claim: &str,
        status: &KubernetesClaimStatus,
    ) -> RemoteSandboxHandle {
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
        self.k8s_request(reqwest::Method::POST, &collection, Some(body))
            .await?;
        let status = self.wait_until_ready(&claim_name).await?;
        self.track_claim(&claim_name);
        Ok(self.handle_from_status(&claim_name, &status))
    }

    async fn connect_sandbox(&self, sandbox_id: &str) -> SandboxResult<RemoteSandboxHandle> {
        let path = self.claim_path(sandbox_id);
        let value = self.k8s_request(reqwest::Method::GET, &path, None).await?;
        let status = parse_claim_status(&value);
        self.track_claim(sandbox_id);
        Ok(self.handle_from_status(sandbox_id, &status))
    }

    async fn kill_sandbox(&self, sandbox_id: &str) -> SandboxResult<()> {
        self.delete_claim_in(&self.options.namespace, sandbox_id)
            .await
    }

    async fn exec_in(
        &self,
        handle: &RemoteSandboxHandle,
        command: &str,
        _cwd: Option<&str>,
        timeout_seconds: u64,
    ) -> SandboxResult<RemoteExecOutput> {
        let base = self.connector_base_url(handle).await?;
        let url = join_url(&base, "execute");
        let body = build_execute_body(command);
        let headers = self.router_headers(handle);
        let client = self.client.clone();
        let retry = self.retry.clone();
        let _ = timeout_seconds;
        let value = with_retries(&retry, move || {
            let client = client.clone();
            let url = url.clone();
            let body = body.clone();
            let headers = headers.clone();
            async move {
                let mut request = client.post(&url);
                for (key, value) in &headers {
                    request = request.header(key, value);
                }
                let response = request
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

    async fn persist_workspace_bytes(
        &self,
        handle: &RemoteSandboxHandle,
        root: &str,
    ) -> SandboxResult<Vec<u8>> {
        match self.native_file_transfer(handle).await? {
            Some(transfer) => self.persist_via_file_api(handle, root, &transfer).await,
            None => persist_tar(self, handle, root).await,
        }
    }

    async fn hydrate_workspace_bytes(
        &self,
        handle: &RemoteSandboxHandle,
        root: &str,
        archive: &[u8],
    ) -> SandboxResult<Option<RemoteSandboxHandle>> {
        match self.native_file_transfer(handle).await? {
            Some(transfer) => {
                self.hydrate_via_file_api(handle, root, archive, &transfer)
                    .await?;
                Ok(None)
            }
            None => {
                hydrate_tar(self, handle, root, archive).await?;
                Ok(None)
            }
        }
    }

    async fn file_transfer(
        &self,
        handle: &RemoteSandboxHandle,
    ) -> Option<Arc<dyn SandboxFileTransfer>> {
        KubernetesBackend::native_file_transfer(self, handle)
            .await
            .ok()
            .flatten()
    }
}

/// [`ExecTransport`] running commands through a live Kubernetes sandbox handle.
///
/// The runtime file API has no move endpoint, so [`KubernetesFileApi`] expresses moves as
/// `mkdir -p && mv` shell commands — the same fallback the Java `KubernetesSandbox` uses.
#[derive(Debug, Clone)]
struct KubernetesHandleExec {
    backend: KubernetesBackend,
    handle: RemoteSandboxHandle,
}

#[async_trait::async_trait]
impl ExecTransport for KubernetesHandleExec {
    async fn exec(&self, command: &str, timeout_seconds: u64) -> SandboxResult<RemoteExecOutput> {
        self.backend
            .exec_in(&self.handle, command, None, timeout_seconds)
            .await
    }
}

/// Kubernetes sandbox client (harness [`agentscope_harness::agent::sandbox::SandboxClient`]).
pub type KubernetesSandboxClient = RemoteSandboxClient;

/// Creates the harness [`agentscope_harness::agent::sandbox::SandboxClient`] for Kubernetes.
pub fn kubernetes_client(
    options: KubernetesSandboxClientOptions,
) -> SandboxResult<KubernetesSandboxClient> {
    Ok(RemoteSandboxClient::new(Arc::new(KubernetesBackend::new(
        options,
    )?)))
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
        assert_eq!(
            body["metadata"]["labels"][CREATED_BY_LABEL],
            CREATED_BY_VALUE
        );
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
    fn connection_strategy_derivation_matches_java_priority() {
        // Explicit option wins over everything.
        let explicit = KubernetesSandboxClientOptions {
            connection_strategy: Some(KubernetesConnectionStrategy::InCluster),
            sandbox_api_url: Some("http://router:7777".to_string()),
            gateway_name: Some("gw".to_string()),
            ..Default::default()
        };
        assert_eq!(
            explicit.resolve_connection_strategy(),
            KubernetesConnectionStrategy::InCluster
        );
        // Then a pre-configured router URL (Java DirectStrategy).
        let direct = KubernetesSandboxClientOptions {
            sandbox_api_url: Some("http://router:7777".to_string()),
            gateway_name: Some("gw".to_string()),
            ..Default::default()
        };
        assert_eq!(
            direct.resolve_connection_strategy(),
            KubernetesConnectionStrategy::Direct
        );
        // Then a gateway name (Java GatewayStrategy).
        let gateway = KubernetesSandboxClientOptions {
            gateway_name: Some("gw".to_string()),
            ..Default::default()
        };
        assert_eq!(
            gateway.resolve_connection_strategy(),
            KubernetesConnectionStrategy::Gateway
        );
        // Java falls back to port-forward; Rust joins the sandbox pod/service directly.
        assert_eq!(
            KubernetesSandboxClientOptions::default().resolve_connection_strategy(),
            KubernetesConnectionStrategy::InCluster
        );
        // Blank values never select a strategy.
        let blank = KubernetesSandboxClientOptions {
            sandbox_api_url: Some("   ".to_string()),
            gateway_name: Some("".to_string()),
            ..Default::default()
        };
        assert_eq!(
            blank.resolve_connection_strategy(),
            KubernetesConnectionStrategy::InCluster
        );
    }

    #[test]
    fn direct_strategy_uses_the_configured_router_url() {
        let options = KubernetesSandboxClientOptions {
            sandbox_api_url: Some("http://router.local:7777/".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_connector_base_url(
                &options,
                KubernetesConnectionStrategy::Direct,
                "sandbox-1",
                Some("10.0.0.5"),
                None
            )
            .unwrap(),
            "http://router.local:7777"
        );
        let missing = KubernetesSandboxClientOptions::default();
        assert!(matches!(
            resolve_connector_base_url(
                &missing,
                KubernetesConnectionStrategy::Direct,
                "sandbox-1",
                None,
                None
            ),
            Err(SandboxExtensionError::Configuration(_))
        ));
    }

    #[test]
    fn in_cluster_strategy_uses_pod_ip_then_service_dns() {
        let options = KubernetesSandboxClientOptions {
            namespace: "team-a".to_string(),
            ..Default::default()
        };
        assert_eq!(
            resolve_connector_base_url(
                &options,
                KubernetesConnectionStrategy::InCluster,
                "sandbox-1",
                Some("10.0.0.5"),
                None
            )
            .unwrap(),
            "http://10.0.0.5:8888"
        );
        // No pod IP -> the per-sandbox service DNS name (Java `InClusterStrategy`).
        assert_eq!(
            resolve_connector_base_url(
                &options,
                KubernetesConnectionStrategy::InCluster,
                "sandbox-1",
                None,
                None
            )
            .unwrap(),
            "http://sandbox-1.team-a.svc.cluster.local:8888"
        );
        // IPv6 pod IPs are bracketed for URL interpolation.
        assert_eq!(
            resolve_connector_base_url(
                &options,
                KubernetesConnectionStrategy::InCluster,
                "sandbox-1",
                Some("fd00::1"),
                None
            )
            .unwrap(),
            "http://[fd00::1]:8888"
        );
        assert!(matches!(
            resolve_connector_base_url(
                &options,
                KubernetesConnectionStrategy::InCluster,
                "  ",
                None,
                None
            ),
            Err(SandboxExtensionError::Configuration(_))
        ));
    }

    #[test]
    fn gateway_strategy_uses_the_external_address_without_a_port() {
        let options = KubernetesSandboxClientOptions {
            gateway_scheme: "https".to_string(),
            ..Default::default()
        };
        assert_eq!(
            resolve_connector_base_url(
                &options,
                KubernetesConnectionStrategy::Gateway,
                "sandbox-1",
                None,
                Some("gw.example.com")
            )
            .unwrap(),
            "https://gw.example.com"
        );
        assert_eq!(
            resolve_connector_base_url(
                &options,
                KubernetesConnectionStrategy::Gateway,
                "sandbox-1",
                None,
                Some("fd00::1")
            )
            .unwrap(),
            "https://[fd00::1]"
        );
        assert!(matches!(
            resolve_connector_base_url(
                &options,
                KubernetesConnectionStrategy::Gateway,
                "sandbox-1",
                None,
                None
            ),
            Err(SandboxExtensionError::Configuration(_))
        ));
    }

    #[test]
    fn router_headers_follow_the_strategy() {
        let direct = KubernetesSandboxClientOptions {
            namespace: "team-a".to_string(),
            sandbox_api_url: Some("http://router:7777".to_string()),
            ..Default::default()
        };
        let backend = KubernetesBackend::new(direct).unwrap();
        let mut extra = BTreeMap::new();
        extra.insert("pod_ip".to_string(), "10.0.0.5".to_string());
        extra.insert("sandbox_name".to_string(), "sandbox-1".to_string());
        let handle = RemoteSandboxHandle::new("agentscope-s1", extra);
        let headers = backend.router_headers(&handle);
        assert!(headers.contains(&(HEADER_SANDBOX_ID.to_string(), "sandbox-1".to_string())));
        assert!(headers.contains(&(HEADER_SANDBOX_NAMESPACE.to_string(), "team-a".to_string())));
        assert!(headers.contains(&(HEADER_SANDBOX_PORT.to_string(), "8888".to_string())));
        assert!(headers.contains(&(HEADER_SANDBOX_POD_IP.to_string(), "10.0.0.5".to_string())));

        // In-cluster traffic goes straight to the pod, so no router headers are injected.
        let in_cluster = KubernetesBackend::new(KubernetesSandboxClientOptions::default()).unwrap();
        assert!(in_cluster.router_headers(&handle).is_empty());
    }

    #[test]
    fn gateway_address_is_parsed_from_the_crd_status() {
        let value = json!({
            "status": {
                "addresses": [
                    {"type": "IPAddress", "value": "1.2.3.4"},
                    {"type": "IPAddress", "value": "5.6.7.8"}
                ]
            }
        });
        assert_eq!(parse_gateway_address(&value).as_deref(), Some("1.2.3.4"));
        assert_eq!(parse_gateway_address(&json!({})), None);
        assert_eq!(
            parse_gateway_address(&json!({"status": {"addresses": []}})),
            None
        );
        assert_eq!(
            parse_gateway_address(&json!({"status": {"addresses": [{"value": "  "}]}})),
            None
        );
    }

    fn file_api_backend(base_url: &str, base_dir: &str) -> KubernetesBackend {
        KubernetesBackend::new(KubernetesSandboxClientOptions {
            namespace: "team-a".to_string(),
            api_url: Some(base_url.to_string()),
            sandbox_api_url: Some(base_url.to_string()),
            file_api_base_dir: base_dir.to_string(),
            max_retries: 1,
            sandbox_ready_timeout_seconds: 1,
            ..Default::default()
        })
        .unwrap()
    }

    fn handle_with_pod_ip(pod_ip: &str) -> RemoteSandboxHandle {
        let mut extra = BTreeMap::new();
        extra.insert("pod_ip".to_string(), pod_ip.to_string());
        extra.insert("sandbox_name".to_string(), "sandbox-1".to_string());
        RemoteSandboxHandle::new("agentscope-s1", extra)
    }

    #[tokio::test]
    async fn exec_sends_router_headers_for_router_strategies() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/execute")
                    .header("x-sandbox-id", "sandbox-1")
                    .header("x-sandbox-namespace", "team-a")
                    .header("x-sandbox-port", "8888")
                    .header("x-sandbox-pod-ip", "10.0.0.5")
                    .json_body(json!({"command": "ls"}));
                then.status(200)
                    .json_body(json!({"stdout": "o", "stderr": "", "exitCode": 0}));
            })
            .await;
        let backend = file_api_backend(&server.base_url(), DEFAULT_WORKSPACE_ROOT);
        let output = backend
            .exec_in(&handle_with_pod_ip("10.0.0.5"), "ls", None, 5)
            .await
            .expect("exec succeeds");
        assert_eq!(output.stdout, "o");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn file_transfer_needs_the_file_api_base_dir() {
        let backend = KubernetesBackend::new(KubernetesSandboxClientOptions {
            sandbox_api_url: Some("http://127.0.0.1:1".to_string()),
            file_api_base_dir: String::new(),
            ..Default::default()
        })
        .unwrap();
        assert!(backend
            .native_file_transfer(&handle_with_pod_ip("10.0.0.5"))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn file_transfer_uploads_and_downloads_under_the_base_dir() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        // `upload_file` creates the parent directory through the exec channel first.
        let mkdir = server
            .mock_async(|when, then| {
                when.method(POST).path("/execute").body_contains("mkdir -p");
                then.status(200)
                    .json_body(json!({"stdout": "", "stderr": "", "exitCode": 0}));
            })
            .await;
        let upload = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/upload")
                    .header("x-sandbox-id", "sandbox-1")
                    .body_contains("hello workspace");
                then.status(200).body("");
            })
            .await;
        let download = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path_matches(Regex::new("^/download/.*").unwrap())
                    .header("x-sandbox-namespace", "team-a");
                then.status(200).body("downloaded");
            })
            .await;
        let backend = file_api_backend(&server.base_url(), DEFAULT_WORKSPACE_ROOT);
        let transfer = backend
            .native_file_transfer(&handle_with_pod_ip("10.0.0.5"))
            .await
            .unwrap()
            .expect("file API is configured");

        assert!(transfer.supports_file_transfer("/workspace/a.txt"));
        assert!(!transfer.supports_file_transfer("/etc/hosts"));
        transfer
            .upload_file("/workspace/a.txt", b"hello workspace")
            .await
            .expect("native upload succeeds");
        upload.assert_async().await;
        mkdir.assert_async().await;
        assert_eq!(
            transfer
                .download_file("/workspace/a.txt")
                .await
                .expect("native download succeeds"),
            b"downloaded".to_vec()
        );
        download.assert_async().await;
    }

    #[tokio::test]
    async fn persist_workspace_uses_the_file_api_when_enabled() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let tar = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/execute")
                    .body_contains("tar --exclude=./.agentscope-tmp");
                then.status(200)
                    .json_body(json!({"stdout": "", "stderr": "", "exitCode": 0}));
            })
            .await;
        // Cleanup (`rm -f`) goes through the same exec channel.
        let cleanup = server
            .mock_async(|when, then| {
                when.method(POST).path("/execute");
                then.status(200)
                    .json_body(json!({"stdout": "", "stderr": "", "exitCode": 0}));
            })
            .await;
        let download = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path_matches(Regex::new("^/download/.*").unwrap());
                then.status(200).body("TARDATA");
            })
            .await;
        let backend = file_api_backend(&server.base_url(), DEFAULT_WORKSPACE_ROOT);
        let archive = backend
            .persist_workspace_bytes(&handle_with_pod_ip("10.0.0.5"), "/workspace")
            .await
            .expect("file-API persist succeeds");
        assert_eq!(archive, b"TARDATA".to_vec());
        tar.assert_async().await;
        assert!(cleanup.hits_async().await >= 1);
        assert!(download.hits_async().await >= 1);
    }

    #[tokio::test]
    async fn persist_workspace_falls_back_to_base64_over_exec() {
        use base64::Engine as _;
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let expected = base64::engine::general_purpose::STANDARD.encode(b"PLAIN");
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/execute")
                    .body_contains("base64 -w0");
                then.status(200).json_body(json!({
                    "stdout": expected,
                    "stderr": "",
                    "exitCode": 0
                }));
            })
            .await;
        // Blank base dir disables the file API (Java `fileApiEnabled`).
        let backend = file_api_backend(&server.base_url(), "");
        let archive = backend
            .persist_workspace_bytes(&handle_with_pod_ip("10.0.0.5"), "/workspace")
            .await
            .expect("exec persist succeeds");
        assert_eq!(archive, b"PLAIN".to_vec());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn hydrate_workspace_uploads_the_archive_then_extracts() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let upload = server
            .mock_async(|when, then| {
                when.method(POST).path("/upload").body_contains("TARDATA");
                then.status(200).body("");
            })
            .await;
        let extract = server
            .mock_async(|when, then| {
                when.method(POST).path("/execute").body_contains("tar -xf");
                then.status(200)
                    .json_body(json!({"stdout": "", "stderr": "", "exitCode": 0}));
            })
            .await;
        // `mkdir -p` for the temp dir and `rm -f` cleanup also use the exec channel.
        let other_exec = server
            .mock_async(|when, then| {
                when.method(POST).path("/execute");
                then.status(200)
                    .json_body(json!({"stdout": "", "stderr": "", "exitCode": 0}));
            })
            .await;
        let backend = file_api_backend(&server.base_url(), DEFAULT_WORKSPACE_ROOT);
        backend
            .hydrate_workspace_bytes(&handle_with_pod_ip("10.0.0.5"), "/workspace", b"TARDATA")
            .await
            .expect("file-API hydrate succeeds");
        upload.assert_async().await;
        extract.assert_async().await;
        assert!(other_exec.hits_async().await >= 1);
    }

    #[tokio::test]
    async fn gateway_strategy_reads_the_address_from_the_crd() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let crd = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/apis/gateway.networking.k8s.io/v1/namespaces/team-a/gateways/gw");
                then.status(200).json_body(json!({
                    "status": {"addresses": [{"type": "IPAddress", "value": "1.2.3.4"}]}
                }));
            })
            .await;
        let backend = KubernetesBackend::new(KubernetesSandboxClientOptions {
            namespace: "team-a".to_string(),
            api_url: Some(server.base_url()),
            gateway_name: Some("gw".to_string()),
            gateway_scheme: "https".to_string(),
            file_api_base_dir: String::new(),
            max_retries: 1,
            ..Default::default()
        })
        .unwrap();
        let base = backend
            .connector_base_url(&handle_with_pod_ip("10.0.0.5"))
            .await
            .expect("gateway base url resolves");
        assert_eq!(base, "https://1.2.3.4");
        crd.assert_async().await;
    }

    #[tokio::test]
    async fn gateway_strategy_prefers_the_injected_address() {
        // No `api_url`, so a CRD read is impossible; the injected address is the escape hatch.
        let backend = KubernetesBackend::new(KubernetesSandboxClientOptions {
            connection_strategy: Some(KubernetesConnectionStrategy::Gateway),
            gateway_name: Some("gw".to_string()),
            gateway_address: Some("gw.example.com".to_string()),
            file_api_base_dir: String::new(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            backend
                .connector_base_url(&handle_with_pod_ip("10.0.0.5"))
                .await
                .unwrap(),
            "http://gw.example.com"
        );
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
        // The `Direct` strategy without a router URL is a configuration error, raised before any
        // network call (the default in-cluster strategy would attempt a DNS lookup instead).
        let backend = KubernetesBackend::new(KubernetesSandboxClientOptions {
            connection_strategy: Some(KubernetesConnectionStrategy::Direct),
            ..Default::default()
        })
        .unwrap();
        let handle = RemoteSandboxHandle::new("claim", BTreeMap::new());
        assert!(matches!(
            backend.exec_in(&handle, "ls", None, 5).await.unwrap_err(),
            SandboxExtensionError::Configuration(_)
        ));
    }

    /// Port-forward test double recording every request.
    #[derive(Debug)]
    struct FakePortForwarder {
        endpoint: std::result::Result<ForwardedEndpoint, String>,
        calls: std::sync::Mutex<Vec<(String, String, String, u16)>>,
    }

    impl FakePortForwarder {
        fn new(endpoint: std::result::Result<ForwardedEndpoint, String>) -> Self {
            Self {
                endpoint,
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, String, String, u16)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl KubernetesPortForwarder for FakePortForwarder {
        async fn forward(
            &self,
            sandbox_id: &str,
            namespace: &str,
            pod_name: &str,
            remote_port: u16,
        ) -> SandboxResult<ForwardedEndpoint> {
            self.calls.lock().unwrap().push((
                sandbox_id.to_string(),
                namespace.to_string(),
                pod_name.to_string(),
                remote_port,
            ));
            self.endpoint
                .clone()
                .map_err(SandboxExtensionError::Transport)
        }
    }

    #[tokio::test]
    async fn port_forward_strategy_uses_the_forwarder_local_endpoint() {
        let forwarder = Arc::new(FakePortForwarder::new(Ok(ForwardedEndpoint::new(
            "127.0.0.1",
            54321,
        ))));
        let backend = KubernetesBackend::new(KubernetesSandboxClientOptions {
            connection_strategy: Some(KubernetesConnectionStrategy::PortForward),
            port_forwarder: Some(forwarder.clone()),
            port_forward_pod: Some("router-abc".to_string()),
            file_api_base_dir: String::new(),
            ..Default::default()
        })
        .unwrap();
        let base = backend
            .connector_base_url(&handle_with_pod_ip("10.0.0.5"))
            .await
            .expect("the forwarded endpoint wins");
        assert_eq!(base, "http://127.0.0.1:54321");
        // The tunnel targets the router pod on the router port, in the router namespace.
        assert_eq!(
            forwarder.calls(),
            vec![(
                "sandbox-1".to_string(),
                DEFAULT_ROUTER_NAMESPACE.to_string(),
                "router-abc".to_string(),
                ROUTER_PORT
            )]
        );
        // The tunnel still fronts the router, so the routing headers are injected.
        let headers = backend.router_headers(&handle_with_pod_ip("10.0.0.5"));
        assert!(headers
            .iter()
            .any(|(k, v)| k == HEADER_SANDBOX_ID && v == "sandbox-1"));
    }

    #[tokio::test]
    async fn port_forward_strategy_without_a_forwarder_is_a_configuration_error() {
        let backend = KubernetesBackend::new(KubernetesSandboxClientOptions {
            connection_strategy: Some(KubernetesConnectionStrategy::PortForward),
            port_forward_pod: Some("router-abc".to_string()),
            ..Default::default()
        })
        .unwrap();
        let err = backend
            .connector_base_url(&handle_with_pod_ip("10.0.0.5"))
            .await
            .expect_err("no forwarder means no base url");
        match err {
            SandboxExtensionError::Configuration(message) => {
                assert!(
                    message.contains("port_forwarder"),
                    "unexpected message: {message}"
                );
                // Crucially, it must not fall back to the in-cluster pod IP URL.
                assert!(!message.contains("10.0.0.5"));
            }
            other => panic!("expected a configuration error, got {other:?}"),
        }
        // The synchronous resolver never silently reroutes a port-forward request either.
        assert!(matches!(
            resolve_connector_base_url(
                &KubernetesSandboxClientOptions::default(),
                KubernetesConnectionStrategy::PortForward,
                "sandbox-1",
                Some("10.0.0.5"),
                None
            ),
            Err(SandboxExtensionError::Configuration(_))
        ));
    }

    #[tokio::test]
    async fn port_forward_forwarder_errors_are_propagated() {
        let forwarder = Arc::new(FakePortForwarder::new(Err(
            "tunnel refused by the API server".to_string(),
        )));
        let backend = KubernetesBackend::new(KubernetesSandboxClientOptions {
            connection_strategy: Some(KubernetesConnectionStrategy::PortForward),
            port_forwarder: Some(forwarder),
            port_forward_pod: Some("router-abc".to_string()),
            ..Default::default()
        })
        .unwrap();
        let err = backend
            .connector_base_url(&handle_with_pod_ip("10.0.0.5"))
            .await
            .expect_err("forwarder failure must surface");
        assert!(
            format!("{err}").contains("tunnel refused by the API server"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn port_forward_router_pod_is_read_from_endpoints() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let endpoints = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/api/v1/namespaces/agent-sandbox-system/endpoints/sandbox-router-svc");
                then.status(200).json_body(json!({
                    "subsets": [{
                        "addresses": [{
                            "ip": "10.0.0.9",
                            "targetRef": {"kind": "Pod", "name": "router-abc"}
                        }]
                    }]
                }));
            })
            .await;
        let forwarder = Arc::new(FakePortForwarder::new(Ok(ForwardedEndpoint::new(
            "127.0.0.1",
            41000,
        ))));
        let backend = KubernetesBackend::new(KubernetesSandboxClientOptions {
            connection_strategy: Some(KubernetesConnectionStrategy::PortForward),
            api_url: Some(server.base_url()),
            port_forwarder: Some(forwarder.clone()),
            file_api_base_dir: String::new(),
            max_retries: 1,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            backend
                .connector_base_url(&handle_with_pod_ip("10.0.0.5"))
                .await
                .unwrap(),
            "http://127.0.0.1:41000"
        );
        endpoints.assert_async().await;
        assert_eq!(forwarder.calls()[0].2, "router-abc");
    }

    #[test]
    fn forwarded_endpoint_renders_http_and_brackets_ipv6() {
        assert_eq!(
            ForwardedEndpoint::new("127.0.0.1", 49152).base_url("http"),
            "http://127.0.0.1:49152"
        );
        assert_eq!(
            ForwardedEndpoint::new(" ::1 ", 9).base_url("http"),
            "http://[::1]:9"
        );
        assert_eq!(
            parse_endpoint_target_pod(&json!({"subsets": [{"addresses": []}, {}]})),
            None
        );
        assert_eq!(
            parse_endpoint_target_pod(&json!({"subsets": [
                {"addresses": [{"ip": "1.2.3.4"}]},
                {"addresses": [{"targetRef": {"name": " router-2 "}}]}
            ]})),
            Some("router-2".to_string())
        );
    }
    // ------------------------------------------- claim listing / registry (Java K8sHelper parity)

    fn claim_list_backend(base_url: &str) -> KubernetesBackend {
        KubernetesBackend::new(KubernetesSandboxClientOptions {
            namespace: "team-a".to_string(),
            api_url: Some(base_url.to_string()),
            file_api_base_dir: String::new(),
            max_retries: 1,
            ..Default::default()
        })
        .unwrap()
    }

    /// `KubernetesSandboxClientOptions` with a one-second readiness budget.
    fn short_budget_backend(base_url: &str) -> KubernetesBackend {
        KubernetesBackend::new(KubernetesSandboxClientOptions {
            namespace: "team-a".to_string(),
            api_url: Some(base_url.to_string()),
            gateway_name: Some("gw".to_string()),
            sandbox_ready_timeout_seconds: 1,
            file_api_base_dir: String::new(),
            max_retries: 1,
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn parse_claim_names_skips_missing_and_blank_names() {
        let names = parse_claim_names(&json!({
            "items": [
                {"metadata": {"name": "claim-b"}},
                {"metadata": {}},
                {},
                {"metadata": {"name": "  "}},
                {"metadata": {"name": "claim-a"}}
            ]
        }));
        // Server order is preserved (Java `listSandboxClaims` does not sort).
        assert_eq!(names, vec!["claim-b".to_string(), "claim-a".to_string()]);
        assert!(parse_claim_names(&json!({})).is_empty());
        assert!(parse_claim_names(&json!({"items": []})).is_empty());
        assert!(parse_claim_names(&Value::Null).is_empty());
    }

    #[test]
    fn claim_warm_pool_name_reads_the_reference() {
        assert_eq!(
            claim_warm_pool_name(&json!({"spec": {"warmPoolRef": {"name": "pool-a"}}})),
            Some("pool-a".to_string())
        );
        assert_eq!(claim_warm_pool_name(&json!({"spec": {}})), None);
        assert_eq!(claim_warm_pool_name(&json!({})), None);
        assert_eq!(
            claim_warm_pool_name(&json!({"spec": {"warmPoolRef": {"name": "  "}}})),
            None
        );
    }

    #[tokio::test]
    async fn list_sandbox_claims_returns_names_in_server_order() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let list = server
            .mock_async(|when, then| {
                when.method(GET).path(
                    "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims",
                );
                then.status(200).json_body(json!({
                    "items": [
                        {"metadata": {"name": "agentscope-b"}},
                        {"metadata": {"name": "agentscope-a"}}
                    ]
                }));
            })
            .await;
        let backend = claim_list_backend(&server.base_url());
        assert_eq!(
            backend.list_sandbox_claims(None).await.unwrap(),
            vec!["agentscope-b".to_string(), "agentscope-a".to_string()]
        );
        list.assert_async().await;
    }

    #[tokio::test]
    async fn list_sandbox_claims_sends_the_label_selector() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        // Only matches when the selector is really sent as `?labelSelector=`; a request without it
        // falls through to httpmock's 404 and the call below fails.
        let list = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path(
                        "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims",
                    )
                    .query_param("labelSelector", "app=demo,tier=agent");
                then.status(200)
                    .json_body(json!({"items": [{"metadata": {"name": "agentscope-c"}}]}));
            })
            .await;
        let backend = claim_list_backend(&server.base_url());
        assert_eq!(
            backend
                .list_sandbox_claims(Some("app=demo,tier=agent"))
                .await
                .unwrap(),
            vec!["agentscope-c".to_string()]
        );
        list.assert_async().await;
    }
    #[tokio::test]
    async fn get_sandbox_claim_returns_none_on_404() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let get = server
            .mock_async(|when, then| {
                when.method(GET).path(
                    "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims/missing",
                );
                then.status(404)
                    .json_body(json!({"kind": "Status", "reason": "NotFound"}));
            })
            .await;
        let backend = claim_list_backend(&server.base_url());
        assert!(backend
            .get_sandbox_claim("missing")
            .await
            .unwrap()
            .is_none());
        get.assert_async().await;
    }

    #[tokio::test]
    async fn get_sandbox_claim_warm_pool_name_reads_the_claim() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let get = server
            .mock_async(|when, then| {
                when.method(GET).path(
                    "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims/c1",
                );
                then.status(200).json_body(
                    json!({"metadata": {"name": "c1"}, "spec": {"warmPoolRef": {"name": "pool-a"}}}),
                );
            })
            .await;
        let backend = claim_list_backend(&server.base_url());
        assert_eq!(
            backend
                .get_sandbox_claim_warm_pool_name("c1")
                .await
                .unwrap(),
            Some("pool-a".to_string())
        );
        get.assert_async().await;
    }

    #[tokio::test]
    async fn get_sandbox_claim_warm_pool_name_errors_when_the_claim_is_missing() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path(
                    "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims/gone",
                );
                then.status(404).body("not found");
            })
            .await;
        let backend = claim_list_backend(&server.base_url());
        let err = backend
            .get_sandbox_claim_warm_pool_name("gone")
            .await
            .unwrap_err();
        // Java throws `SandboxNotFoundException` with this message.
        assert!(
            err.to_string()
                .contains("SandboxClaim 'gone' not found in namespace 'team-a'."),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn delete_sandbox_claim_tolerates_404_and_blank_names() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let delete = server
            .mock_async(|when, then| {
                when.method(DELETE).path(
                    "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims/c1",
                );
                then.status(200)
                    .json_body(json!({"kind": "Status", "status": "Success"}));
            })
            .await;
        let gone = server
            .mock_async(|when, then| {
                when.method(DELETE).path(
                    "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims/gone",
                );
                then.status(404).body("not found");
            })
            .await;
        let backend = claim_list_backend(&server.base_url());
        backend.delete_sandbox_claim("c1").await.unwrap();
        // Already deleted claims are a no-op, exactly like `K8sHelper#deleteSandboxClaim`.
        backend.delete_sandbox_claim("gone").await.unwrap();
        // Blank names never reach the API.
        backend.delete_sandbox_claim("   ").await.unwrap();
        delete.assert_async().await;
        gone.assert_async().await;
    }

    #[tokio::test]
    async fn active_claims_track_attach_and_forget_deletes() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path(
                    "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims/c1",
                );
                then.status(200)
                    .json_body(json!({"metadata": {"name": "c1"}}));
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(DELETE).path(
                    "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims/c1",
                );
                then.status(200).body("");
            })
            .await;
        let backend = claim_list_backend(&server.base_url());
        assert!(backend.list_active_sandboxes().is_empty());
        backend.connect_sandbox("c1").await.unwrap();
        // Java keys the registry as `namespace/claimName`.
        assert_eq!(
            backend.list_active_sandboxes(),
            vec!["team-a/c1".to_string()]
        );
        backend.kill_sandbox("c1").await.unwrap();
        assert!(backend.list_active_sandboxes().is_empty());
    }

    #[tokio::test]
    async fn delete_all_removes_every_tracked_claim() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        for (claim, path) in [
            (
                "c1",
                "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims/c1",
            ),
            (
                "c2",
                "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims/c2",
            ),
        ] {
            server
                .mock_async(move |when, then| {
                    when.method(GET).path(path);
                    then.status(200)
                        .json_body(json!({"metadata": {"name": claim}}));
                })
                .await;
        }
        let delete_c1 = server
            .mock_async(|when, then| {
                when.method(DELETE).path(
                    "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims/c1",
                );
                then.status(200).body("");
            })
            .await;
        let delete_c2 = server
            .mock_async(|when, then| {
                when.method(DELETE).path(
                    "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims/c2",
                );
                then.status(200).body("");
            })
            .await;
        let backend = claim_list_backend(&server.base_url());
        backend.connect_sandbox("c1").await.unwrap();
        backend.connect_sandbox("c2").await.unwrap();
        assert_eq!(backend.list_active_sandboxes().len(), 2);
        backend.delete_all().await.unwrap();
        delete_c1.assert_async().await;
        delete_c2.assert_async().await;
        assert!(backend.list_active_sandboxes().is_empty());
    }

    #[tokio::test]
    async fn delete_all_reports_the_first_failure_but_clears_the_registry() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path(
                    "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims/c1",
                );
                then.status(200)
                    .json_body(json!({"metadata": {"name": "c1"}}));
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(DELETE).path(
                    "/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/team-a/sandboxclaims/c1",
                );
                // 400 is not retryable, so the failure surfaces on the first attempt.
                then.status(400).body("bad request");
            })
            .await;
        let backend = claim_list_backend(&server.base_url());
        backend.connect_sandbox("c1").await.unwrap();
        let err = backend.delete_all().await.unwrap_err();
        assert_eq!(err.status(), Some(400));
        assert!(backend.list_active_sandboxes().is_empty());
    }

    // ------------------------------------------------- gateway address wait (Java GatewayStrategy)

    #[tokio::test]
    async fn gateway_address_wait_polls_until_the_address_appears() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let polls = std::sync::Arc::new(AtomicUsize::new(0));
        let polls_ref = polls.clone();
        let address = wait_for_gateway_address_with(
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(1),
            move || {
                let polls = polls_ref.clone();
                async move {
                    let seen = polls.fetch_add(1, Ordering::SeqCst);
                    // Mirrors a Gateway that only publishes an address once programmed.
                    Ok((seen >= 2).then(|| "1.2.3.4".to_string()))
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(address, Some("1.2.3.4".to_string()));
        assert_eq!(polls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gateway_address_wait_gives_up_after_the_budget() {
        let address = wait_for_gateway_address_with(
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(1),
            || async { Ok(None) },
        )
        .await
        .unwrap();
        assert_eq!(address, None);
        // Fetch errors are propagated instead of being reported as a timeout.
        let err = wait_for_gateway_address_with(
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(1),
            || async { Err(SandboxExtensionError::Transport("boom".to_string())) },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("boom"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn gateway_without_a_published_address_times_out_with_the_java_message() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let crd = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/apis/gateway.networking.k8s.io/v1/namespaces/team-a/gateways/gw");
                then.status(200)
                    .json_body(json!({"status": {"addresses": []}}));
            })
            .await;
        let backend = short_budget_backend(&server.base_url());
        let err = backend.wait_for_gateway_address().await.unwrap_err();
        assert!(
            err.to_string()
                .contains("Timed out waiting for gateway address: gw"),
            "unexpected error: {err}"
        );
        // Polled more than once before giving up (a plain `get` would only see one hit).
        assert!(crd.hits_async().await >= 2);
    }
}
