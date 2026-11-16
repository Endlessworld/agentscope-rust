//! AgentRun (Alibaba Cloud) sandbox backend.
//!
//! Mirrors the Java `io.agentscope.extensions.sandbox.agentrun` package:
//!
//! * data-plane lifecycle (`POST/GET/DELETE /sandboxes`, readiness polling), and
//! * command execution through the AgentRun MCP `process_exec_cmd` tool.
//!
//! Command execution speaks the MCP *streamable HTTP* transport: every remote sandbox first goes
//! through the `initialize` handshake (announcing [`MCP_PROTOCOL_VERSIONS`]), the `Mcp-Session-Id`
//! returned by the server is cached per sandbox id and replayed on every `tools/call`, so
//! session-scoped servers accept the call (mirrors the Java `AgentRunMcpChannel#connect`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::{Client, Response};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::error::{SandboxErrorCode, SandboxExtensionError, SandboxResult};
use crate::http::{bypass_proxy_for_loopback, join_url};
use crate::remote::{
    persist_tar, RemoteBackend, RemoteExecOutput, RemoteSandboxClient, RemoteSandboxHandle,
};
use crate::retry::{with_retries, RetryPolicy};

/// Default workspace root inside an AgentRun sandbox container.
pub const DEFAULT_WORKSPACE_ROOT: &str = "/home/agentscope/workspace";
/// Maximum number of instance-level OSS mounts AgentRun accepts per sandbox.
pub const MAX_OSS_MOUNTS: usize = 5;
/// Allowed mount-dir prefixes for NAS and OSS instance mounts.
pub const ALLOWED_MOUNT_PREFIXES: [&str; 3] = ["/home/", "/mnt/", "/data/"];
/// MCP tool name for shell-style command execution.
pub const TOOL_EXEC: &str = "process_exec_cmd";
/// Readiness poll interval, in milliseconds.
pub const READY_POLL_INTERVAL_MS: u64 = 1500;
/// MCP protocol versions this backend announces, newest first (mirrors the Java
/// `McpClientBuilder#protocolVersions` list).
pub const MCP_PROTOCOL_VERSIONS: [&str; 2] = ["2025-03-26", "2024-11-05"];
/// Header carrying the MCP streamable-HTTP session id.
pub const MCP_SESSION_ID_HEADER: &str = "Mcp-Session-Id";
/// MCP client name announced in the `initialize` request.
pub const MCP_CLIENT_NAME: &str = "agentscope-rust-sandbox";
/// `Accept` header required by the MCP streamable-HTTP transport (JSON + SSE).
pub const MCP_ACCEPT: &str = "application/json, text/event-stream";
/// JSON-RPC id of the `initialize` request (the `tools/call` uses id `1`).
pub const MCP_INITIALIZE_REQUEST_ID: u64 = 0;
/// Length of an AgentRun sandbox id: 26 Crockford Base32 characters (130 bits).
pub const SANDBOX_ID_LENGTH: usize = 26;
/// Crockford Base32 alphabet used for AgentRun sandbox ids.
pub const SANDBOX_ID_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Handle `extra` key recording whether the workspace lives on a persistent mount.
///
/// Mirrors the Java `AgentRunSandboxState#workspaceOnNas`: it is derived once at create/connect and
/// persisted with the state so a resumed session keeps the NAS semantics even if the client
/// options are rebuilt without the mount configuration.
pub const WORKSPACE_ON_NAS_EXTRA_KEY: &str = "workspace_on_nas";

/// NAS mount configuration (mirrors `AgentRunNasMountConfig`).
#[derive(Debug, Clone, Default)]
pub struct AgentRunNasMountConfig {
    /// NAS server address.
    pub server_addr: Option<String>,
    /// Mount directory inside the sandbox.
    pub mount_dir: Option<String>,
    /// Remote path on the NAS.
    pub remote_path: Option<String>,
    /// Whether to enable TLS for the mount.
    pub enable_tls: bool,
}

/// OSS mount configuration (mirrors `AgentRunOssMountConfig`).
#[derive(Debug, Clone, Default)]
pub struct AgentRunOssMountConfig {
    /// Bucket name.
    pub bucket_name: Option<String>,
    /// Path inside the bucket.
    pub bucket_path: Option<String>,
    /// OSS endpoint.
    pub endpoint: Option<String>,
    /// Mount directory inside the sandbox.
    pub mount_dir: Option<String>,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

/// Options for the AgentRun sandbox client (mirrors `AgentRunSandboxClientOptions`).
#[derive(Debug, Clone)]
pub struct AgentRunSandboxClientOptions {
    /// AgentRun API key (`X-API-Key`).
    pub api_key: Option<String>,
    /// Alibaba Cloud account id (used to derive the data-plane URL).
    pub account_id: Option<String>,
    /// Alibaba Cloud region (used to derive the data-plane URL).
    pub region: Option<String>,
    /// Explicit data-plane base URL (overrides account/region derivation).
    pub data_plane_base_url: Option<String>,
    /// Sandbox template name.
    pub template_name: Option<String>,
    /// MCP server URL (required for command execution).
    pub mcp_server_url: Option<String>,
    /// MCP endpoint path.
    pub mcp_endpoint: String,
    /// Sandbox idle timeout in seconds.
    pub sandbox_idle_timeout_seconds: u64,
    /// Optional NAS mount.
    pub nas_config: Option<AgentRunNasMountConfig>,
    /// Optional OSS mounts (at most [`MAX_OSS_MOUNTS`]).
    pub oss_mount_configs: Vec<AgentRunOssMountConfig>,
    /// Absolute workspace root inside the sandbox.
    pub workspace_root: String,
    /// TCP connect timeout in seconds.
    pub connect_timeout_seconds: u64,
    /// Read timeout in seconds.
    pub read_timeout_seconds: u64,
    /// Maximum retry attempts.
    pub max_retries: u32,
    /// Max seconds to wait for readiness.
    pub ready_timeout_seconds: u64,
}

impl Default for AgentRunSandboxClientOptions {
    fn default() -> Self {
        Self {
            api_key: None,
            account_id: None,
            region: None,
            data_plane_base_url: None,
            template_name: None,
            mcp_server_url: None,
            mcp_endpoint: "/mcp".to_string(),
            sandbox_idle_timeout_seconds: 1800,
            nas_config: None,
            oss_mount_configs: Vec::new(),
            workspace_root: DEFAULT_WORKSPACE_ROOT.to_string(),
            connect_timeout_seconds: 30,
            read_timeout_seconds: 120,
            max_retries: 3,
            ready_timeout_seconds: 300,
        }
    }
}

/// Returns whether a mount dir starts with one of [`ALLOWED_MOUNT_PREFIXES`].
pub fn is_allowed_mount_dir(value: &str) -> bool {
    ALLOWED_MOUNT_PREFIXES
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

impl AgentRunSandboxClientOptions {
    /// Whether this configuration puts the workspace on a persistent NAS/OSS mount.
    ///
    /// Mirrors the Java `AgentRunSandboxClient#isWorkspaceUnderMounts` call inside `create`.
    pub fn workspace_on_nas(&self) -> bool {
        workspace_on_nas(
            &self.workspace_root,
            self.nas_config.as_ref(),
            &self.oss_mount_configs,
        )
    }
}

/// Returns whether `workspace_root` lives under a persistent NAS/OSS mount.
///
/// Mirrors the Java `AgentRunSandboxClient#isWorkspaceUnderMounts`: a NAS mount covers the root by
/// prefix, an OSS mount only when it is writable (a read-only bucket cannot back a workspace).
/// When true the workspace survives sandbox deletion, so the adapter skips tar persistence and
/// "resume" is just a recreate with the same deterministic id and mount configuration.
pub fn workspace_on_nas(
    workspace_root: &str,
    nas: Option<&AgentRunNasMountConfig>,
    oss: &[AgentRunOssMountConfig],
) -> bool {
    let root = workspace_root.trim();
    if root.is_empty() {
        return false;
    }
    if let Some(mount_dir) = nas.and_then(|c| c.mount_dir.as_deref()) {
        if root.starts_with(mount_dir) {
            return true;
        }
    }
    oss.iter().any(|mount| {
        !mount.read_only
            && mount
                .mount_dir
                .as_deref()
                .is_some_and(|dir| root.starts_with(dir))
    })
}

/// Validates option invariants that do not require a live sandbox.
pub fn validate_options(options: &AgentRunSandboxClientOptions) -> SandboxResult<()> {
    let missing = |field: &str| {
        SandboxExtensionError::Configuration(format!(
            "AgentRun {field} is required (AgentRunSandboxClientOptions)"
        ))
    };
    if options
        .api_key
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        return Err(missing("API key"));
    }
    if options
        .template_name
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        return Err(missing("template name"));
    }
    if options
        .mcp_server_url
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        return Err(missing("MCP server URL"));
    }
    if let Some(nas) = &options.nas_config {
        if let Some(dir) = nas.mount_dir.as_deref().filter(|d| !d.trim().is_empty()) {
            if !is_allowed_mount_dir(dir) {
                return Err(SandboxExtensionError::Configuration(format!(
                    "nasConfig.mountDir must start with one of {ALLOWED_MOUNT_PREFIXES:?} but was: {dir}"
                )));
            }
        }
    }
    if options.oss_mount_configs.len() > MAX_OSS_MOUNTS {
        return Err(SandboxExtensionError::Configuration(format!(
            "AgentRun supports at most {MAX_OSS_MOUNTS} OSS mounts per sandbox; got {}",
            options.oss_mount_configs.len()
        )));
    }
    for (i, mount) in options.oss_mount_configs.iter().enumerate() {
        if let Some(dir) = mount.mount_dir.as_deref().filter(|d| !d.trim().is_empty()) {
            if !is_allowed_mount_dir(dir) {
                return Err(SandboxExtensionError::Configuration(format!(
                    "ossMountConfigs[{i}].mountDir must start with one of {ALLOWED_MOUNT_PREFIXES:?} but was: {dir}"
                )));
            }
        }
    }
    Ok(())
}

/// Resolves the data-plane base URL from an explicit value or account/region.
pub fn resolve_data_plane_base_url(
    options: &AgentRunSandboxClientOptions,
) -> SandboxResult<String> {
    if let Some(url) = options
        .data_plane_base_url
        .as_deref()
        .filter(|u| !u.trim().is_empty())
    {
        return Ok(url.trim_end_matches('/').to_string());
    }
    match (
        options
            .account_id
            .as_deref()
            .filter(|v| !v.trim().is_empty()),
        options.region.as_deref().filter(|v| !v.trim().is_empty()),
    ) {
        (Some(account), Some(region)) => Ok(format!(
            "https://{account}.agentrun-data.{region}.aliyuncs.com"
        )),
        _ => Err(SandboxExtensionError::Configuration(
            "AgentRun requires accountId+region or an explicit dataPlaneBaseUrl".to_string(),
        )),
    }
}

/// Encodes `data` as Crockford Base32 (MSB-first 5-bit groups, zero-padded tail).
pub fn crockford_base32(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 8 / 5 + 2);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for byte in data {
        acc = (acc << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            let index = ((acc >> (bits - 5)) & 0x1f) as usize;
            out.push(SANDBOX_ID_ALPHABET[index] as char);
            bits -= 5;
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        let index = ((acc << (5 - bits)) & 0x1f) as usize;
        out.push(SANDBOX_ID_ALPHABET[index] as char);
    }
    out
}

/// Derives the deterministic AgentRun sandbox id for `session_id`.
///
/// Mirrors the Java `AgentRunSandboxClient#deriveSandboxId`: `base32(SHA-256(sessionId))`
/// truncated to [`SANDBOX_ID_LENGTH`] Crockford Base32 characters (the "AgentRun-acceptable
/// shape"). Keeping it stable means the same session always maps onto the same sandbox, so a
/// retried create is idempotent instead of failing on AgentRun's id-shape validation.
pub fn derive_sandbox_id(session_id: &str) -> String {
    let digest = Sha256::digest(session_id.as_bytes());
    crockford_base32(digest.as_slice())
        .chars()
        .take(SANDBOX_ID_LENGTH)
        .collect()
}

/// Builds the `POST /sandboxes` request body for a deterministic sandbox id.
pub fn build_create_sandbox_body(
    options: &AgentRunSandboxClientOptions,
    sandbox_id: &str,
) -> SandboxResult<Value> {
    let mut body = json!({
        "sandboxId": sandbox_id,
        "templateName": options.template_name.clone().unwrap_or_default(),
        "sandboxIdleTimeoutSeconds": options.sandbox_idle_timeout_seconds,
    });
    if let Some(nas) = &options.nas_config {
        let server_addr = nas.server_addr.as_deref().unwrap_or("").trim();
        let mount_dir = nas.mount_dir.as_deref().unwrap_or("").trim();
        if !server_addr.is_empty() && !mount_dir.is_empty() {
            body["nasMountConfig"] = json!({
                "mountPoints": [{
                    "serverAddr": server_addr,
                    "mountDir": mount_dir,
                    "remotePath": nas.remote_path.clone().unwrap_or_default(),
                    "enableTLS": nas.enable_tls,
                }]
            });
        }
    }
    if !options.oss_mount_configs.is_empty() {
        let mounts: Vec<Value> = options
            .oss_mount_configs
            .iter()
            .map(|m| {
                json!({
                    "bucketName": m.bucket_name.clone().unwrap_or_default(),
                    "bucketPath": m.bucket_path.clone().unwrap_or_default(),
                    "endpoint": m.endpoint.clone().unwrap_or_default(),
                    "mountDir": m.mount_dir.clone().unwrap_or_default(),
                    "readOnly": m.read_only,
                })
            })
            .collect();
        body["ossMountConfig"] = json!({ "mountPoints": mounts });
    }
    Ok(body)
}

/// Results of an AgentRun MCP exec call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRunExecResult {
    /// Process exit code.
    pub exit_code: i32,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
}

impl Default for AgentRunExecResult {
    fn default() -> Self {
        Self {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }
}

/// Builds a JSON-RPC 2.0 `tools/call` request for the AgentRun exec tool.
pub fn build_mcp_tool_call(
    request_id: u64,
    tool: &str,
    command: &str,
    cwd: Option<&str>,
    timeout_seconds: u64,
) -> Value {
    let mut arguments = json!({
        "command": command,
        "timeout": timeout_seconds.max(1),
    });
    if let Some(cwd) = cwd.filter(|c| !c.trim().is_empty()) {
        arguments["cwd"] = Value::String(cwd.to_string());
    }
    json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    })
}

/// Builds the JSON-RPC 2.0 `initialize` request for the MCP streamable-HTTP handshake.
pub fn build_mcp_initialize(request_id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "initialize",
        "params": {
            "protocolVersion": MCP_PROTOCOL_VERSIONS[0],
            "capabilities": {},
            "clientInfo": {"name": MCP_CLIENT_NAME, "version": env!("CARGO_PKG_VERSION")},
        }
    })
}

/// Returns the protocol version a server negotiated in its `initialize` response.
///
/// A missing `result.protocolVersion` falls back to the newest version this backend announces;
/// a version outside [`MCP_PROTOCOL_VERSIONS`] is rejected (the MCP spec requires the client to
/// disconnect when it cannot speak the server's version).
pub fn mcp_protocol_version(response: &Value) -> SandboxResult<String> {
    let version = response
        .get("result")
        .and_then(|result| result.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or(MCP_PROTOCOL_VERSIONS[0]);
    if MCP_PROTOCOL_VERSIONS.contains(&version) {
        Ok(version.to_string())
    } else {
        Err(SandboxExtensionError::Runtime(
            SandboxErrorCode::RemoteTransportError,
            format!(
                "AgentRun MCP server negotiated unsupported protocol version {version}; \
                 supported: {MCP_PROTOCOL_VERSIONS:?}"
            ),
        ))
    }
}

/// Extracts the text payload from an MCP `tools/call` JSON-RPC response.
pub fn extract_mcp_text(response: &Value) -> Option<String> {
    let result = response.get("result")?;
    let content = result.get("content")?.as_array()?;
    for item in content {
        if item.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                return Some(text.to_string());
            }
        }
    }
    None
}

/// Returns whether the MCP response flagged a tool error.
pub fn mcp_is_error(response: &Value) -> bool {
    response
        .get("result")
        .and_then(|r| r.get("isError"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Parses an AgentRun exec payload (JSON object or plain text) into an exec result.
pub fn parse_exec_payload(text: &str) -> AgentRunExecResult {
    let trimmed = text.trim();
    if let Some(brace) = trimmed.find('{') {
        let json_text = &trimmed[brace..];
        if let Ok(node) = serde_json::from_str::<Value>(json_text) {
            let result = node
                .get("result")
                .filter(|r| r.is_object())
                .unwrap_or(&node);
            let exit_code = result
                .get("exitCode")
                .or_else(|| result.get("exit_code"))
                .and_then(Value::as_i64);
            if exit_code.is_some() {
                return AgentRunExecResult {
                    exit_code: exit_code.unwrap_or(0) as i32,
                    stdout: result
                        .get("stdout")
                        .or_else(|| result.get("output"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    stderr: result
                        .get("stderr")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                };
            }
        }
    }
    AgentRunExecResult {
        exit_code: 0,
        stdout: trimmed.to_string(),
        stderr: String::new(),
    }
}

/// Unwraps the AgentRun `{"data": {...}}` response envelope.
///
/// The data plane wraps every payload in a `data` object, so deserializing the top level either
/// fails outright or yields an empty structure (#1907). Responses without the envelope (control
/// plane, MCP JSON-RPC) are returned unchanged.
pub fn unwrap_data_envelope(value: Value) -> Value {
    if let Value::Object(map) = &value {
        if let Some(data) = map.get("data") {
            if data.is_object() || data.is_array() {
                return data.clone();
            }
        }
    }
    value
}

/// Reads the mount points the server reports for a sandbox response.
///
/// Returns `(mount_dir, writable)` pairs. An empty list means the server did not echo any mount,
/// i.e. the configured mounts were not applied — the persistence path must key on that rather
/// than on the local prefix check alone (#2538/#2382).
pub fn confirmed_mount_dirs(value: &Value) -> Vec<(String, bool)> {
    let value = unwrap_data_envelope(value.clone());
    let mut dirs = Vec::new();
    for (key, writable_by_default) in [("nasMountConfig", true), ("ossMountConfig", false)] {
        let Some(points) = value
            .get(key)
            .and_then(|c| c.get("mountPoints"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for point in points {
            let Some(dir) = point.get("mountDir").and_then(Value::as_str) else {
                continue;
            };
            let dir = dir.trim();
            if dir.is_empty() {
                continue;
            }
            let read_only = point
                .get("readOnly")
                .and_then(Value::as_bool)
                .unwrap_or(!writable_by_default);
            dirs.push((dir.to_string(), !read_only));
        }
    }
    dirs
}

/// Whether the server confirmed a persistent mount that covers the workspace root.
pub fn workspace_on_confirmed_mount(workspace_root: &str, mounts: &[(String, bool)]) -> bool {
    let root = workspace_root.trim();
    !root.is_empty()
        && mounts
            .iter()
            .any(|(dir, writable)| *writable && root.starts_with(dir.as_str()))
}

/// Decides whether the workspace is durable for this sandbox.
///
/// The server's own mount report wins when it has one: a create response that lists mounts means
/// the mounts are live, and one that lists none means the workspace is sandbox-local despite the
/// configuration. Without any report the configured prefix check is the only evidence available.
pub fn workspace_mount_effective(value: &Value, options: &AgentRunSandboxClientOptions) -> bool {
    let mounts = confirmed_mount_dirs(value);
    if mounts.is_empty() {
        options.workspace_on_nas()
    } else {
        workspace_on_confirmed_mount(&options.workspace_root, &mounts)
    }
}

/// Extracts the sandbox status from a `GET /sandboxes/{id}` response.
pub fn parse_sandbox_status(value: &Value) -> Option<String> {
    let value = unwrap_data_envelope(value.clone());
    value
        .get("status")
        .and_then(Value::as_str)
        .or_else(|| value.get("state").and_then(Value::as_str))
        .map(|s| s.to_string())
}

/// AgentRun [`RemoteBackend`] implementation.
#[derive(Debug)]
pub struct AgentRunBackend {
    options: AgentRunSandboxClientOptions,
    client: Client,
    retry: RetryPolicy,
    base_url: String,
    /// MCP streamable-HTTP session ids, keyed by remote sandbox id (filled by
    /// [`AgentRunBackend::ensure_mcp_session`]).
    mcp_sessions: Mutex<BTreeMap<String, String>>,
    /// Workspace archives captured by [`AgentRunBackend::kill_sandbox`] when the mount was never
    /// confirmed by the server (the sandbox is about to be deleted, so these bytes are the only
    /// copy left). Reachable through [`AgentRunBackend::take_backup`].
    backups: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl AgentRunBackend {
    /// Builds a backend after validating options and resolving the data-plane URL.
    pub fn new(options: AgentRunSandboxClientOptions) -> SandboxResult<Self> {
        validate_options(&options)?;
        let base_url = resolve_data_plane_base_url(&options)?;
        let client = bypass_proxy_for_loopback(
            Client::builder()
                .connect_timeout(Duration::from_secs(options.connect_timeout_seconds))
                .timeout(Duration::from_secs(options.read_timeout_seconds)),
            &[base_url.as_str()],
        )
        .build()
        .map_err(|e| SandboxExtensionError::Transport(format!("failed to build client: {e}")))?;
        let retry = RetryPolicy::agentrun(options.max_retries);
        Ok(Self {
            options,
            client,
            retry,
            base_url,
            mcp_sessions: Mutex::new(BTreeMap::new()),
            backups: Mutex::new(BTreeMap::new()),
        })
    }

    /// Takes the workspace archive captured when
    /// [`AgentRunBackend::kill_sandbox`] could not confirm a durable mount.
    pub async fn take_backup(&self, sandbox_id: &str) -> Option<Vec<u8>> {
        self.backups.lock().await.remove(sandbox_id)
    }

    /// Returns the configured options.
    pub fn options(&self) -> &AgentRunSandboxClientOptions {
        &self.options
    }

    fn api_key(&self) -> SandboxResult<String> {
        self.options
            .api_key
            .clone()
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| {
                SandboxExtensionError::Configuration(
                    "AgentRun API key is required (AgentRunSandboxClientOptions::api_key)"
                        .to_string(),
                )
            })
    }

    fn mcp_url(&self) -> SandboxResult<String> {
        let base = self
            .options
            .mcp_server_url
            .as_deref()
            .filter(|u| !u.trim().is_empty())
            .ok_or_else(|| {
                SandboxExtensionError::Configuration(
                    "AgentRun MCP server URL is required".to_string(),
                )
            })?;
        Ok(join_url(base, &self.options.mcp_endpoint))
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

    async fn request_json(
        &self,
        method: reqwest::Method,
        url: String,
        body: Option<Value>,
    ) -> SandboxResult<Value> {
        let api_key = self.api_key()?;
        let account_id = self.options.account_id.clone();
        let client = self.client.clone();
        let retry = self.retry.clone();
        with_retries(&retry, move || {
            let client = client.clone();
            let url = url.clone();
            let method = method.clone();
            let api_key = api_key.clone();
            let account_id = account_id.clone();
            let body = body.clone();
            async move {
                let mut request = client
                    .request(method, &url)
                    .header("X-API-Key", api_key.clone());
                if let Some(account_id) = account_id.filter(|a| !a.trim().is_empty()) {
                    request = request.header("X-Acs-Parent-Id", account_id);
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

    /// Returns the cached MCP session id for `sandbox_id`, when the handshake already ran.
    pub async fn mcp_session_id(&self, sandbox_id: &str) -> Option<String> {
        self.mcp_sessions.lock().await.get(sandbox_id).cloned()
    }

    /// Posts one JSON-RPC message to the MCP endpoint.
    ///
    /// Returns the `Mcp-Session-Id` the server echoed back (if any) alongside the decoded body,
    /// so callers can cache it without a second round trip.
    async fn send_mcp(
        &self,
        sandbox_id: &str,
        body: &Value,
        session_id: Option<&str>,
    ) -> SandboxResult<(Option<String>, Value)> {
        let url = self.mcp_url()?;
        let api_key = self.api_key()?;
        let account_id = self.options.account_id.clone();
        let client = self.client.clone();
        let retry = self.retry.clone();
        let sandbox_id = sandbox_id.to_string();
        let session_id = session_id.map(str::to_string);
        let body = body.clone();
        with_retries(&retry, move || {
            let client = client.clone();
            let url = url.clone();
            let api_key = api_key.clone();
            let account_id = account_id.clone();
            let sandbox_id = sandbox_id.clone();
            let session_id = session_id.clone();
            let body = body.clone();
            async move {
                let mut request = client
                    .post(&url)
                    .header("X-API-Key", api_key)
                    .header("X-AgentRun-Sandbox-Id", sandbox_id)
                    .header("Accept", MCP_ACCEPT);
                if let Some(account_id) = account_id.filter(|a| !a.trim().is_empty()) {
                    request = request.header("X-Acs-Parent-Id", account_id);
                }
                if let Some(session_id) = session_id.as_deref().filter(|s| !s.trim().is_empty()) {
                    request = request.header(MCP_SESSION_ID_HEADER, session_id);
                }
                let response = request
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
                let session_id = response
                    .headers()
                    .get(MCP_SESSION_ID_HEADER)
                    .and_then(|value| value.to_str().ok())
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty());
                let value = Self::decode_json(response, &url).await?;
                Ok((session_id, value))
            }
        })
        .await
    }

    /// Runs the MCP `initialize` handshake for `sandbox_id` and caches the session id.
    ///
    /// The handshake runs at most once per remote sandbox: `exec_in` awaits this before the
    /// `tools/call`, so a session-scoped server never sees a stateless call. Mirrors the Java
    /// `AgentRunMcpChannel#connect` (streamable-HTTP transport carrying the API key and the
    /// `X-Acs-Parent-Id` header, then `initialize()`).
    async fn ensure_mcp_session(&self, sandbox_id: &str) -> SandboxResult<String> {
        // The lock is held across the handshake so concurrent execs share a single session.
        let mut sessions = self.mcp_sessions.lock().await;
        if let Some(session_id) = sessions.get(sandbox_id) {
            return Ok(session_id.clone());
        }
        let (session_id, response) = self
            .send_mcp(
                sandbox_id,
                &build_mcp_initialize(MCP_INITIALIZE_REQUEST_ID),
                None,
            )
            .await?;
        if let Some(error) = response.get("error") {
            return Err(SandboxExtensionError::Runtime(
                SandboxErrorCode::RemoteTransportError,
                format!("AgentRun MCP initialize failed: {error}"),
            ));
        }
        mcp_protocol_version(&response)?;
        let session_id = session_id.ok_or_else(|| {
            SandboxExtensionError::Runtime(
                SandboxErrorCode::RemoteTransportError,
                format!(
                    "AgentRun MCP initialize returned no {MCP_SESSION_ID_HEADER} header; \
                     the session is required before {TOOL_EXEC} can be called"
                ),
            )
        })?;
        sessions.insert(sandbox_id.to_string(), session_id.clone());
        Ok(session_id)
    }

    async fn wait_until_ready(&self, sandbox_id: &str) -> SandboxResult<()> {
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(self.options.ready_timeout_seconds.max(1));
        let mut last: Option<String> = None;
        loop {
            let url = join_url(&self.base_url, &format!("/sandboxes/{sandbox_id}"));
            if let Ok(value) = self.request_json(reqwest::Method::GET, url, None).await {
                if let Some(status) = parse_sandbox_status(&value) {
                    let upper = status.to_uppercase();
                    if upper.contains("READY") || upper.contains("RUNNING") {
                        return Ok(());
                    }
                    if upper.contains("FAILED") || upper.contains("TERMINATED") {
                        return Err(SandboxExtensionError::Runtime(
                            SandboxErrorCode::WorkspaceStartError,
                            format!(
                                "AgentRun sandbox entered terminal state {status}: {sandbox_id}"
                            ),
                        ));
                    }
                    last = Some(status);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(SandboxExtensionError::Runtime(
                    SandboxErrorCode::WorkspaceStartError,
                    format!(
                        "AgentRun sandbox did not become READY in time (last status={}): {sandbox_id}",
                        last.unwrap_or_else(|| "unknown".to_string())
                    ),
                ));
            }
            tokio::time::sleep(Duration::from_millis(READY_POLL_INTERVAL_MS)).await;
        }
    }

    fn handle_for(&self, sandbox_id: &str) -> RemoteSandboxHandle {
        self.handle_for_with_mount(sandbox_id, self.options.workspace_on_nas())
    }

    /// Builds a handle whose persisted NAS flag reflects what the server actually reported.
    ///
    /// The flag decides whether [`AgentRunBackend::persist_workspace_bytes`] archives the
    /// workspace or trusts the mount, so it must follow the server's answer rather than the local
    /// prefix check alone (#2538/#2382).
    fn handle_for_with_mount(
        &self,
        sandbox_id: &str,
        mount_effective: bool,
    ) -> RemoteSandboxHandle {
        let mut extra = BTreeMap::new();
        if let Some(template) = &self.options.template_name {
            extra.insert("template_name".to_string(), template.clone());
        }
        if let Some(mcp) = &self.options.mcp_server_url {
            extra.insert("mcp_server_url".to_string(), mcp.clone());
        }
        extra.insert(
            WORKSPACE_ON_NAS_EXTRA_KEY.to_string(),
            mount_effective.to_string(),
        );
        RemoteSandboxHandle::new(sandbox_id, extra)
    }

    /// Whether the server explicitly confirms a durable mount covering the workspace.
    ///
    /// Only a positive report counts here: a response that echoes no mount at all (or a mount
    /// elsewhere) is "unconfirmed", so the caller keeps the bytes instead of trusting a mount that
    /// may not exist. An unreadable answer is unconfirmed for the same reason.
    async fn mount_confirmed(&self, sandbox_id: &str) -> bool {
        let url = join_url(&self.base_url, &format!("/sandboxes/{sandbox_id}"));
        match self.request_json(reqwest::Method::GET, url, None).await {
            Ok(response) => workspace_on_confirmed_mount(
                &self.options.workspace_root,
                &confirmed_mount_dirs(&response),
            ),
            Err(e) => {
                tracing::warn!(
                    "AgentRun sandbox {sandbox_id}: could not confirm the workspace mount ({e}); \
                     treating it as unconfirmed"
                );
                false
            }
        }
    }
}

#[async_trait::async_trait]
impl RemoteBackend for AgentRunBackend {
    fn backend_name(&self) -> &'static str {
        "agentrun"
    }

    fn endpoint_base(&self) -> String {
        self.base_url.clone()
    }

    fn workspace_root(&self) -> &str {
        &self.options.workspace_root
    }

    async fn create_sandbox(&self, session_id: &str) -> SandboxResult<RemoteSandboxHandle> {
        // Java derives a deterministic 26-char sandbox id from the session id, so a repeated
        // create for the same session targets the same sandbox instead of an invalid 36-char id.
        let sandbox_id = derive_sandbox_id(session_id);
        // A re-created sandbox must not replay the previous incarnation's MCP session.
        self.mcp_sessions.lock().await.remove(&sandbox_id);
        let url = join_url(&self.base_url, "/sandboxes");
        let body = build_create_sandbox_body(&self.options, &sandbox_id)?;
        let response = self
            .request_json(reqwest::Method::POST, url, Some(body))
            .await?;
        self.wait_until_ready(&sandbox_id).await?;
        // Trust the server's own mount report over the local prefix check (#2538/#2382).
        let mount_effective = workspace_mount_effective(&response, &self.options);
        Ok(self.handle_for_with_mount(&sandbox_id, mount_effective))
    }

    /// Adopts the deterministic sandbox for `session_id` when it already exists.
    ///
    /// The first exec runs the MCP `initialize` handshake, which starts the sandbox through
    /// "resolve or create"; without this probe that start issued a second `CreateSandbox` for an
    /// id that already exists (#2070). A missing or terminal instance is left to
    /// [`Self::create_sandbox`].
    async fn reuse_sandbox(&self, session_id: &str) -> SandboxResult<Option<RemoteSandboxHandle>> {
        let sandbox_id = derive_sandbox_id(session_id);
        let url = join_url(&self.base_url, &format!("/sandboxes/{sandbox_id}"));
        let response = match self.request_json(reqwest::Method::GET, url, None).await {
            Ok(response) => response,
            // Any client-side rejection means "no reusable instance": 404 is the documented one,
            // and creating afterwards surfaces the real error (bad key, bad template) instead of
            // failing the start on a probe.
            Err(SandboxExtensionError::Http { status, .. }) if (400..500).contains(&status) => {
                return Ok(None)
            }
            Err(e) => return Err(e),
        };
        let status = parse_sandbox_status(&response)
            .unwrap_or_default()
            .to_uppercase();
        if ["FAILED", "TERMINATED", "DELETED", "STOPPED"]
            .iter()
            .any(|terminal| status.contains(terminal))
        {
            return Ok(None);
        }
        let mount_effective = workspace_mount_effective(&response, &self.options);
        if !status.contains("READY") && !status.contains("RUNNING") {
            // Still starting: resolve means "hand back a usable sandbox", so wait for it.
            self.wait_until_ready(&sandbox_id).await?;
        }
        Ok(Some(
            self.handle_for_with_mount(&sandbox_id, mount_effective),
        ))
    }

    async fn connect_sandbox(&self, sandbox_id: &str) -> SandboxResult<RemoteSandboxHandle> {
        let url = join_url(&self.base_url, &format!("/sandboxes/{sandbox_id}"));
        let response = self.request_json(reqwest::Method::GET, url, None).await?;
        let mount_effective = workspace_mount_effective(&response, &self.options);
        Ok(self.handle_for_with_mount(sandbox_id, mount_effective))
    }

    async fn kill_sandbox(&self, sandbox_id: &str) -> SandboxResult<()> {
        // A configured mount that the server never confirmed does not actually keep the
        // workspace, so the bytes have to be captured *before* the DELETE discards them
        // (#2538/#2382). The confirmation comes from the live GET, since a handle may have been
        // persisted by a process that saw a different answer.
        if self.options.workspace_on_nas() && !self.mount_confirmed(sandbox_id).await {
            let handle = self.handle_for(sandbox_id);
            match persist_tar(self, &handle, &self.options.workspace_root).await {
                Ok(bytes) if !bytes.is_empty() => {
                    tracing::warn!(
                        "AgentRun sandbox {sandbox_id}: mount was not confirmed by the server; \
                         captured {} workspace bytes before deletion (no OSS uploader is \
                         configured, use `take_backup` to persist them)",
                        bytes.len()
                    );
                    self.backups
                        .lock()
                        .await
                        .insert(sandbox_id.to_string(), bytes);
                }
                Ok(_) => tracing::warn!(
                    "AgentRun sandbox {sandbox_id}: mount was not confirmed and the workspace \
                     archive is empty; nothing to back up"
                ),
                // Keeping the sandbox is the only way to keep the workspace: the caller still
                // holds its reference, so a retry can back it up properly.
                Err(e) => {
                    tracing::warn!(
                        "AgentRun sandbox {sandbox_id}: workspace backup before deletion failed \
                         ({e}); keeping the sandbox and its snapshot reference"
                    );
                    return Err(e);
                }
            }
        }
        // The MCP session dies with the sandbox, so drop it before the DELETE.
        self.mcp_sessions.lock().await.remove(sandbox_id);
        let url = join_url(&self.base_url, &format!("/sandboxes/{sandbox_id}"));
        let api_key = self.api_key()?;
        let account_id = self.options.account_id.clone();
        let client = self.client.clone();
        let retry = self.retry.clone();
        with_retries(&retry, move || {
            let client = client.clone();
            let url = url.clone();
            let api_key = api_key.clone();
            let account_id = account_id.clone();
            async move {
                let mut request = client.delete(&url).header("X-API-Key", api_key);
                if let Some(account_id) = account_id.filter(|a| !a.trim().is_empty()) {
                    request = request.header("X-Acs-Parent-Id", account_id);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
                let status = response.status().as_u16();
                if status == 404 || response.status().is_success() {
                    Ok(())
                } else {
                    let body = response.text().await.unwrap_or_default();
                    Err(SandboxExtensionError::http(status, body))
                }
            }
        })
        .await
    }

    async fn exec_in(
        &self,
        handle: &RemoteSandboxHandle,
        command: &str,
        cwd: Option<&str>,
        timeout_seconds: u64,
    ) -> SandboxResult<RemoteExecOutput> {
        // MCP streamable HTTP: the sandbox must be initialized (and its session id replayed)
        // before `tools/call`, otherwise session-scoped servers reject the call.
        let session_id = self.ensure_mcp_session(&handle.id).await?;
        let request = build_mcp_tool_call(1, TOOL_EXEC, command, cwd, timeout_seconds);
        let (_, response) = self
            .send_mcp(&handle.id, &request, Some(&session_id))
            .await?;

        if mcp_is_error(&response) {
            let message = extract_mcp_text(&response).unwrap_or_default();
            return Err(SandboxExtensionError::Runtime(
                SandboxErrorCode::CommandExecutionError,
                format!("AgentRun MCP {TOOL_EXEC} error: {message}"),
            ));
        }
        let text = extract_mcp_text(&response).unwrap_or_default();
        let parsed = parse_exec_payload(&text);
        let mut output = RemoteExecOutput {
            exit_code: parsed.exit_code,
            stdout: parsed.stdout,
            stderr: parsed.stderr,
            truncated: false,
        };
        // Java `AgentRunSandbox#doExec`: 512 KiB stdout budget + `truncated` flag.
        output.truncate_stdout();
        Ok(output)
    }

    async fn persist_workspace_bytes(
        &self,
        handle: &RemoteSandboxHandle,
        root: &str,
    ) -> SandboxResult<Vec<u8>> {
        // SANDBOX-08: a workspace under a persistent mount is durable on its own, so archiving it
        // into the snapshot would duplicate (and on restore overwrite) the mount contents. Java
        // `AgentRunSandbox#doPersistWorkspace` returns an empty stream, which the hydration side
        // treats as the "nothing to restore" marker.
        let on_nas = handle
            .extra
            .get(WORKSPACE_ON_NAS_EXTRA_KEY)
            .map(|v| v == "true")
            .unwrap_or_else(|| self.options.workspace_on_nas());
        if on_nas {
            return Ok(Vec::new());
        }
        persist_tar(self, handle, root).await
    }
}

/// AgentRun sandbox client (harness [`agentscope_harness::agent::sandbox::SandboxClient`]).
pub type AgentRunSandboxClient = RemoteSandboxClient;

/// Creates the harness [`agentscope_harness::agent::sandbox::SandboxClient`] for AgentRun.
pub fn agentrun_client(
    options: AgentRunSandboxClientOptions,
) -> SandboxResult<AgentRunSandboxClient> {
    Ok(RemoteSandboxClient::new(Arc::new(AgentRunBackend::new(
        options,
    )?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_options() -> AgentRunSandboxClientOptions {
        AgentRunSandboxClientOptions {
            api_key: Some("key".to_string()),
            template_name: Some("tpl".to_string()),
            mcp_server_url: Some("https://mcp.example.com".to_string()),
            data_plane_base_url: Some("https://data.example.com".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_prefers_explicit_url() {
        let options = base_options();
        assert_eq!(
            resolve_data_plane_base_url(&options).unwrap(),
            "https://data.example.com"
        );
    }

    #[test]
    fn resolve_derives_from_account_and_region() {
        let options = AgentRunSandboxClientOptions {
            data_plane_base_url: None,
            account_id: Some("acc".to_string()),
            region: Some("cn-hangzhou".to_string()),
            ..base_options()
        };
        assert_eq!(
            resolve_data_plane_base_url(&options).unwrap(),
            "https://acc.agentrun-data.cn-hangzhou.aliyuncs.com"
        );
    }

    #[test]
    fn resolve_requires_account_and_region() {
        let options = AgentRunSandboxClientOptions {
            data_plane_base_url: None,
            ..base_options()
        };
        assert!(resolve_data_plane_base_url(&options).is_err());
    }

    #[test]
    fn validate_rejects_missing_fields_and_bad_mounts() {
        let mut options = base_options();
        options.api_key = None;
        assert!(validate_options(&options).is_err());

        let mut options = base_options();
        options.nas_config = Some(AgentRunNasMountConfig {
            server_addr: Some("s".to_string()),
            mount_dir: Some("/etc/passwd".to_string()),
            ..Default::default()
        });
        assert!(validate_options(&options).is_err());

        let mut options = base_options();
        options.oss_mount_configs = (0..6)
            .map(|_| AgentRunOssMountConfig {
                mount_dir: Some("/home/x".to_string()),
                ..Default::default()
            })
            .collect();
        assert!(validate_options(&options).is_err());
    }

    #[test]
    fn workspace_on_nas_covers_persistent_mounts() {
        // SANDBOX-08 / Java `AgentRunSandboxClient#isWorkspaceUnderMounts`.
        let nas = AgentRunNasMountConfig {
            mount_dir: Some("/home/nas".to_string()),
            ..Default::default()
        };
        assert!(workspace_on_nas("/home/nas", Some(&nas), &[]));
        assert!(workspace_on_nas("/home/nas/work", Some(&nas), &[]));
        // Java uses a plain `startsWith` (no path-boundary check), so a sibling sharing the textual
        // prefix counts as covered too — kept byte-identical to avoid a silent semantic drift.
        assert!(workspace_on_nas("/home/nas-other", Some(&nas), &[]));
        // A blank root has no mount semantics, and no mounts means sandbox-local.
        assert!(!workspace_on_nas("   ", Some(&nas), &[]));
        assert!(!workspace_on_nas("/home/agentscope/workspace", None, &[]));

        // A writable OSS mount covers the root; a read-only one cannot back a workspace.
        let writable = AgentRunOssMountConfig {
            mount_dir: Some("/data/oss".to_string()),
            ..Default::default()
        };
        assert!(workspace_on_nas(
            "/data/oss/ws",
            None,
            std::slice::from_ref(&writable)
        ));
        let read_only = AgentRunOssMountConfig {
            read_only: true,
            ..writable
        };
        assert!(!workspace_on_nas("/data/oss/ws", None, &[read_only]));
    }

    #[test]
    fn options_workspace_on_nas_mirrors_the_mount_configuration() {
        let mut options = base_options();
        assert!(!options.workspace_on_nas());
        options.workspace_root = "/home/nas/ws".to_string();
        options.nas_config = Some(AgentRunNasMountConfig {
            mount_dir: Some("/home/nas".to_string()),
            ..Default::default()
        });
        assert!(options.workspace_on_nas());
    }

    #[test]
    fn data_envelope_is_unwrapped_before_parsing() {
        // #1907: the data plane answers `{"data": {...}}`, so the top level is not the payload.
        let wrapped = json!({"data": {"status": "READY"}});
        assert_eq!(parse_sandbox_status(&wrapped).as_deref(), Some("READY"));
        assert_eq!(unwrap_data_envelope(wrapped), json!({"status": "READY"}));
        // Responses without an envelope stay as they are.
        let plain = json!({"status": "READY"});
        assert_eq!(parse_sandbox_status(&plain).as_deref(), Some("READY"));
        // A non-object `data` member is not an envelope.
        let odd = json!({"data": "text"});
        assert_eq!(unwrap_data_envelope(odd.clone()), odd);
    }

    #[test]
    fn server_reported_mounts_decide_the_workspace_durability() {
        // #2538/#2382: the server's report wins over the local prefix check.
        let options = AgentRunSandboxClientOptions {
            workspace_root: "/home/nas/ws".to_string(),
            nas_config: Some(AgentRunNasMountConfig {
                mount_dir: Some("/home/nas".to_string()),
                ..Default::default()
            }),
            ..base_options()
        };
        // No report at all: the configuration is the only evidence.
        assert!(workspace_mount_effective(&json!({}), &options));
        // An echoed mount confirms it.
        let echoed =
            json!({"data": {"nasMountConfig": {"mountPoints": [{"mountDir": "/home/nas"}]}}});
        assert!(workspace_mount_effective(&echoed, &options));
        assert_eq!(
            confirmed_mount_dirs(&echoed),
            vec![("/home/nas".to_string(), true)]
        );
        // A report naming only an unrelated read-only mount says the workspace is local.
        let elsewhere = json!({"ossMountConfig": {"mountPoints": [{"mountDir": "/data/oss", "readOnly": true}]}});
        assert!(!workspace_mount_effective(&elsewhere, &options));
        assert_eq!(
            confirmed_mount_dirs(&elsewhere),
            vec![("/data/oss".to_string(), false)]
        );
    }

    #[tokio::test]
    async fn create_sandbox_reads_the_data_envelope() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let session_id = "envelope-session";
        let expected = derive_sandbox_id(session_id);
        let create = server
            .mock_async(|when, then| {
                when.method(POST).path("/sandboxes");
                then.status(200)
                    .json_body(json!({"data": {"sandboxId": "ack"}}));
            })
            .await;
        let poll_path = format!("/sandboxes/{expected}");
        let poll = server
            .mock_async(|when, then| {
                when.method(GET).path(poll_path.as_str());
                then.status(200)
                    .json_body(json!({"data": {"status": "READY"}}));
            })
            .await;
        let backend = AgentRunBackend::new(AgentRunSandboxClientOptions {
            data_plane_base_url: Some(server.base_url()),
            // Fail fast instead of waiting the default 300s if the envelope is not unwrapped.
            ready_timeout_seconds: 1,
            max_retries: 1,
            ..base_options()
        })
        .unwrap();
        let handle = backend.create_sandbox(session_id).await.unwrap();
        create.assert_hits_async(1).await;
        poll.assert_hits_async(1).await;
        assert_eq!(handle.id, expected);
    }

    #[tokio::test]
    async fn reuse_reports_an_existing_sandbox_without_creating_one() {
        // #2070: the MCP-triggered start must adopt the sandbox a previous create built.
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let session_id = "reuse-session";
        let expected = derive_sandbox_id(session_id);
        let probe_path = format!("/sandboxes/{expected}");
        let live = server
            .mock_async(|when, then| {
                when.method(GET).path(probe_path.as_str());
                then.status(200)
                    .json_body(json!({"data": {"status": "READY"}}));
            })
            .await;
        let create = server
            .mock_async(|when, then| {
                when.method(POST).path("/sandboxes");
                then.status(200).json_body(json!({"status": "READY"}));
            })
            .await;
        let backend = AgentRunBackend::new(AgentRunSandboxClientOptions {
            data_plane_base_url: Some(server.base_url()),
            max_retries: 1,
            ..base_options()
        })
        .unwrap();
        let handle = backend
            .reuse_sandbox(session_id)
            .await
            .unwrap()
            .expect("the live sandbox is adopted");
        assert_eq!(handle.id, expected);
        live.assert_hits_async(1).await;
        create.assert_hits_async(0).await;

        // A missing sandbox falls through to a create.
        let other = derive_sandbox_id("missing-session");
        let missing_path = format!("/sandboxes/{other}");
        server
            .mock_async(|when, then| {
                when.method(GET).path(missing_path.as_str());
                then.status(404);
            })
            .await;
        assert!(backend
            .reuse_sandbox("missing-session")
            .await
            .unwrap()
            .is_none());

        // A terminal sandbox must be recreated instead of adopted.
        let dead = derive_sandbox_id("dead-session");
        let dead_path = format!("/sandboxes/{dead}");
        server
            .mock_async(|when, then| {
                when.method(GET).path(dead_path.as_str());
                then.status(200).json_body(json!({"status": "TERMINATED"}));
            })
            .await;
        assert!(backend
            .reuse_sandbox("dead-session")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn kill_backs_up_an_unconfirmed_workspace_before_deleting() {
        // #2538/#2382: a configured mount the server never confirms is not durable, so the bytes
        // are captured before the DELETE.
        use base64::Engine as _;
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let archive = b"tarball".to_vec();
        let exec_payload = json!({
            "exitCode": 0,
            "stdout": base64::engine::general_purpose::STANDARD.encode(&archive),
        })
        .to_string();
        let live = server
            .mock_async(|when, then| {
                when.method(GET).path("/sandboxes/sbx-nas");
                then.status(200).json_body(json!({"status": "READY"}));
            })
            .await;
        let init = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/mcp")
                    .body_contains("\"method\":\"initialize\"");
                then.status(200)
                    .header("Mcp-Session-Id", "s1")
                    .json_body(json!({
                        "jsonrpc": "2.0",
                        "id": 0,
                        "result": {"protocolVersion": "2025-03-26"}
                    }));
            })
            .await;
        let call = server
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/mcp")
                    .body_contains("\"method\":\"tools/call\"");
                then.status(200).json_body(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {"content": [{"type": "text", "text": exec_payload}], "isError": false}
                }));
            })
            .await;
        let delete = server
            .mock_async(|when, then| {
                when.method(DELETE).path("/sandboxes/sbx-nas");
                then.status(200);
            })
            .await;
        let backend = AgentRunBackend::new(AgentRunSandboxClientOptions {
            workspace_root: "/home/nas/ws".to_string(),
            nas_config: Some(AgentRunNasMountConfig {
                mount_dir: Some("/home/nas".to_string()),
                ..Default::default()
            }),
            data_plane_base_url: Some(server.base_url()),
            mcp_server_url: Some(server.base_url()),
            max_retries: 1,
            ..base_options()
        })
        .unwrap();

        backend.kill_sandbox("sbx-nas").await.unwrap();
        live.assert_hits_async(1).await;
        init.assert_hits_async(1).await;
        call.assert_hits_async(1).await;
        delete.assert_hits_async(1).await;
        assert_eq!(
            backend.take_backup("sbx-nas").await.as_deref(),
            Some(archive.as_slice()),
            "the archive is kept for the caller instead of being deleted"
        );
    }

    #[tokio::test]
    async fn kill_keeps_the_sandbox_when_the_backup_fails() {
        // A failed backup must not be followed by a DELETE: the sandbox is the only copy left.
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let live = server
            .mock_async(|when, then| {
                when.method(GET).path("/sandboxes/sbx-nas");
                then.status(200).json_body(json!({"status": "READY"}));
            })
            .await;
        let broken = server
            .mock_async(|when, then| {
                when.method(POST).path("/mcp");
                then.status(500).body("boom");
            })
            .await;
        let delete = server
            .mock_async(|when, then| {
                when.method(DELETE).path("/sandboxes/sbx-nas");
                then.status(200);
            })
            .await;
        let backend = AgentRunBackend::new(AgentRunSandboxClientOptions {
            workspace_root: "/home/nas/ws".to_string(),
            nas_config: Some(AgentRunNasMountConfig {
                mount_dir: Some("/home/nas".to_string()),
                ..Default::default()
            }),
            data_plane_base_url: Some(server.base_url()),
            mcp_server_url: Some(server.base_url()),
            max_retries: 1,
            ..base_options()
        })
        .unwrap();
        assert!(backend.kill_sandbox("sbx-nas").await.is_err());
        live.assert_hits_async(1).await;
        broken.assert_hits_async(1).await;
        delete.assert_hits_async(0).await;
    }

    #[tokio::test]
    async fn nas_workspace_persists_and_hydrates_without_any_traffic() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let any = server
            .mock_async(|when, then| {
                when.any_request();
                then.status(200).body("{}");
            })
            .await;
        let backend = AgentRunBackend::new(AgentRunSandboxClientOptions {
            workspace_root: "/home/nas/ws".to_string(),
            data_plane_base_url: Some(server.base_url()),
            nas_config: Some(AgentRunNasMountConfig {
                mount_dir: Some("/home/nas".to_string()),
                ..Default::default()
            }),
            ..base_options()
        })
        .unwrap();
        let handle = backend.handle_for(&derive_sandbox_id("sess-nas"));
        assert_eq!(
            handle
                .extra
                .get(WORKSPACE_ON_NAS_EXTRA_KEY)
                .map(String::as_str),
            Some("true"),
            "the NAS flag is persisted with the handle"
        );

        // No tar command, no HTTP: the mount itself is the persistence.
        let archive = backend
            .persist_workspace_bytes(&handle, "/home/nas/ws")
            .await
            .unwrap();
        assert!(archive.is_empty());
        // The empty marker hydrates to a no-op instead of extracting an empty tar.
        assert!(backend
            .hydrate_workspace_bytes(&handle, "/home/nas/ws", &[])
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            any.hits_async().await,
            0,
            "a NAS-backed workspace must not hit the data plane"
        );
    }

    #[test]
    fn create_body_includes_mounts() {
        let options = AgentRunSandboxClientOptions {
            nas_config: Some(AgentRunNasMountConfig {
                server_addr: Some("nas.local".to_string()),
                mount_dir: Some("/home/nas".to_string()),
                remote_path: Some("/remote".to_string()),
                enable_tls: true,
            }),
            oss_mount_configs: vec![AgentRunOssMountConfig {
                bucket_name: Some("bucket".to_string()),
                bucket_path: Some("path".to_string()),
                endpoint: Some("oss.example.com".to_string()),
                mount_dir: Some("/data/oss".to_string()),
                read_only: true,
            }],
            ..base_options()
        };
        let body = build_create_sandbox_body(&options, "sbx-1").unwrap();
        assert_eq!(body["sandboxId"], "sbx-1");
        assert_eq!(body["templateName"], "tpl");
        assert_eq!(body["sandboxIdleTimeoutSeconds"], 1800);
        assert_eq!(
            body["nasMountConfig"]["mountPoints"][0]["serverAddr"],
            "nas.local"
        );
        assert_eq!(body["nasMountConfig"]["mountPoints"][0]["enableTLS"], true);
        assert_eq!(
            body["ossMountConfig"]["mountPoints"][0]["bucketName"],
            "bucket"
        );
        assert_eq!(body["ossMountConfig"]["mountPoints"][0]["readOnly"], true);
    }

    #[test]
    fn create_body_omits_empty_mounts() {
        let options = AgentRunSandboxClientOptions {
            nas_config: Some(AgentRunNasMountConfig::default()),
            ..base_options()
        };
        let body = build_create_sandbox_body(&options, "sbx").unwrap();
        assert!(body.get("nasMountConfig").is_none());
        assert!(body.get("ossMountConfig").is_none());
    }

    #[test]
    fn mcp_tool_call_shape() {
        let request = build_mcp_tool_call(7, TOOL_EXEC, "echo hi", Some("/home/a"), 0);
        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["id"], 7);
        assert_eq!(request["method"], "tools/call");
        assert_eq!(request["params"]["name"], "process_exec_cmd");
        assert_eq!(request["params"]["arguments"]["command"], "echo hi");
        assert_eq!(request["params"]["arguments"]["cwd"], "/home/a");
        assert_eq!(request["params"]["arguments"]["timeout"], 1);
    }

    #[test]
    fn extract_mcp_text_reads_text_content() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"content": [{"type": "text", "text": "hello"}], "isError": false}
        });
        assert_eq!(extract_mcp_text(&response).as_deref(), Some("hello"));
        assert!(!mcp_is_error(&response));
    }

    #[test]
    fn parse_exec_payload_variants() {
        let json_payload = parse_exec_payload(r#"{"exitCode":0,"stdout":"out","stderr":"err"}"#);
        assert_eq!(json_payload.exit_code, 0);
        assert_eq!(json_payload.stdout, "out");
        assert_eq!(json_payload.stderr, "err");

        let nested =
            parse_exec_payload("banner\n{\"result\":{\"exit_code\":3,\"output\":\"nested\"}}");
        assert_eq!(nested.exit_code, 3);
        assert_eq!(nested.stdout, "nested");

        let plain = parse_exec_payload("just text");
        assert_eq!(plain.exit_code, 0);
        assert_eq!(plain.stdout, "just text");
    }

    #[test]
    fn parse_sandbox_status_reads_status_or_state() {
        assert_eq!(
            parse_sandbox_status(&json!({"status": "READY"})).as_deref(),
            Some("READY")
        );
        assert_eq!(
            parse_sandbox_status(&json!({"state": "RUNNING"})).as_deref(),
            Some("RUNNING")
        );
        assert_eq!(parse_sandbox_status(&json!({})), None);
    }

    #[test]
    fn backend_construction_validates_options() {
        assert!(AgentRunBackend::new(AgentRunSandboxClientOptions::default()).is_err());
        assert!(AgentRunBackend::new(base_options()).is_ok());
    }

    fn mock_backend(server: &httpmock::MockServer, account_id: Option<&str>) -> AgentRunBackend {
        AgentRunBackend::new(AgentRunSandboxClientOptions {
            api_key: Some("key".to_string()),
            account_id: account_id.map(str::to_string),
            template_name: Some("tpl".to_string()),
            data_plane_base_url: Some(server.base_url()),
            mcp_server_url: Some(server.base_url()),
            max_retries: 1,
            ..Default::default()
        })
        .expect("mock backend options are valid")
    }

    #[test]
    fn sandbox_ids_are_26_crockford_chars() {
        // Fixed vectors derived independently (SHA-256 + Crockford Base32), so both the encoder
        // and the 26-char truncation are pinned by a literal.
        assert_eq!(
            derive_sandbox_id("3f8b2c1e-9a4d-4e6f-8b7a-1c2d3e4f5a6b"),
            "GBE25AJQ63KPTM7JFSCJPW0PHD"
        );
        assert_eq!(
            derive_sandbox_id("session-abc"),
            "VYCAGN20GK5T3FXWY07TX8CKQX"
        );
        assert_eq!(
            derive_sandbox_id("test-session"),
            "951Y8EY06K4BZ471RA4NF5NSAK"
        );

        let session = "6f9a0f2f-0000-4000-8000-000000000000";
        let id = derive_sandbox_id(session);
        assert_eq!(id.len(), SANDBOX_ID_LENGTH);
        assert!(id.bytes().all(|b| SANDBOX_ID_ALPHABET.contains(&b)));
        // Same session -> same id (idempotent re-create); a different one diverges.
        assert_eq!(id, derive_sandbox_id(session));
        assert_ne!(
            id,
            derive_sandbox_id("6f9a0f2f-0000-4000-8000-000000000001")
        );
    }

    #[test]
    fn crockford_base32_pads_the_tail() {
        assert_eq!(crockford_base32(b"\x00\x00"), "0000");
        assert_eq!(crockford_base32(b"\xff"), "ZW");
        assert_eq!(crockford_base32(b"foobar").len(), 10);
        assert_eq!(crockford_base32(b"foobar"), "CSQPYRK1E8");
    }

    #[test]
    fn mcp_initialize_request_shape() {
        let request = build_mcp_initialize(0);
        assert_eq!(
            request,
            json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {
                        "name": MCP_CLIENT_NAME,
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }
            })
        );
        assert!(MCP_PROTOCOL_VERSIONS.contains(&"2024-11-05"));
    }

    #[test]
    fn mcp_protocol_version_is_validated() {
        let supported = json!({"result": {"protocolVersion": "2024-11-05"}});
        assert_eq!(mcp_protocol_version(&supported).unwrap(), "2024-11-05");
        // Missing field falls back to the newest announced version.
        assert_eq!(
            mcp_protocol_version(&json!({"result": {}})).unwrap(),
            MCP_PROTOCOL_VERSIONS[0]
        );
        assert!(
            mcp_protocol_version(&json!({"result": {"protocolVersion": "1999-01-01"}})).is_err()
        );
    }

    #[tokio::test]
    async fn create_sandbox_sends_the_derived_26_char_id() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let session_id = "3f8b2c1e-9a4d-4e6f-8b7a-1c2d3e4f5a6b";
        let expected = derive_sandbox_id(session_id);
        assert_eq!(expected, "GBE25AJQ63KPTM7JFSCJPW0PHD");
        let body_fragment = format!("\"sandboxId\":\"{expected}\"");
        let poll_path = format!("/sandboxes/{expected}");
        let create = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/sandboxes")
                    .header("x-api-key", "key")
                    .body_contains(body_fragment.as_str());
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(json!({"sandboxId": "ack"}));
            })
            .await;
        let poll = server
            .mock_async(|when, then| {
                when.method(GET).path(poll_path.as_str());
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(json!({"status": "READY"}));
            })
            .await;
        let backend = mock_backend(&server, None);
        let handle = backend.create_sandbox(session_id).await.unwrap();
        create.assert_hits_async(1).await;
        poll.assert_hits_async(1).await;
        assert_eq!(handle.id, expected);
        assert_eq!(handle.id.len(), SANDBOX_ID_LENGTH);
        assert!(handle.id.bytes().all(|b| SANDBOX_ID_ALPHABET.contains(&b)));
    }

    #[tokio::test]
    async fn exec_initializes_the_mcp_session_before_the_tool_call() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let init = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/mcp")
                    .header("x-api-key", "key")
                    .header("x-acs-parent-id", "acc")
                    .header("x-agentrun-sandbox-id", "sbx-1")
                    .header("accept", "application/json, text/event-stream")
                    .body_contains("\"method\":\"initialize\"");
                then.status(200)
                    .header("content-type", "application/json")
                    .header("Mcp-Session-Id", "mcp-sess-1")
                    .json_body(json!({
                        "jsonrpc": "2.0",
                        "id": 0,
                        "result": {
                            "protocolVersion": "2025-03-26",
                            "capabilities": {},
                            "serverInfo": {"name": "agentrun", "version": "1.0"}
                        }
                    }));
            })
            .await;
        let call = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/mcp")
                    .header("x-api-key", "key")
                    .header("mcp-session-id", "mcp-sess-1")
                    .header("x-agentrun-sandbox-id", "sbx-1")
                    .body_contains("\"method\":\"tools/call\"");
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {"content": [{"type": "text", "text": "hi"}], "isError": false}
                    }));
            })
            .await;

        let delete = server
            .mock_async(|when, then| {
                when.method(DELETE).path("/sandboxes/sbx-1");
                then.status(200);
            })
            .await;

        let backend = mock_backend(&server, Some("acc"));
        let handle = RemoteSandboxHandle::new("sbx-1", BTreeMap::new());
        let output = backend.exec_in(&handle, "echo hi", None, 5).await.unwrap();
        // The `tools/call` mock only matches when the session id from `initialize` is replayed,
        // so both hits prove the handshake ran first.
        init.assert_hits_async(1).await;
        call.assert_hits_async(1).await;
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, "hi");
        assert_eq!(
            backend.mcp_session_id("sbx-1").await.as_deref(),
            Some("mcp-sess-1")
        );

        // The cached session is reused: no second `initialize`.
        backend
            .exec_in(&handle, "echo again", None, 5)
            .await
            .unwrap();
        init.assert_hits_async(1).await;
        call.assert_hits_async(2).await;

        // Destroying the sandbox drops the cached session: a re-create must re-initialize.
        backend.kill_sandbox("sbx-1").await.unwrap();
        delete.assert_hits_async(1).await;
        assert!(backend.mcp_session_id("sbx-1").await.is_none());
    }

    #[tokio::test]
    async fn exec_without_a_mcp_session_reports_the_missing_header() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let init = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/mcp")
                    .body_contains("\"method\":\"initialize\"");
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(json!({
                        "jsonrpc": "2.0",
                        "id": 0,
                        "result": {"protocolVersion": "2025-03-26"}
                    }));
            })
            .await;
        let call = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/mcp")
                    .body_contains("\"method\":\"tools/call\"");
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {"content": [{"type": "text", "text": "unexpected"}]}
                    }));
            })
            .await;

        let backend = mock_backend(&server, None);
        let handle = RemoteSandboxHandle::new("sbx-2", BTreeMap::new());
        let err = backend
            .exec_in(&handle, "echo hi", None, 5)
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Mcp-Session-Id"), "got {message}");
        init.assert_hits_async(1).await;
        // No unauthenticated `tools/call` may be issued without a session.
        call.assert_hits_async(0).await;
        assert!(backend.mcp_session_id("sbx-2").await.is_none());
    }

    #[tokio::test]
    async fn exec_truncates_stdout_at_512_kib() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/mcp")
                    .body_contains("\"method\":\"initialize\"");
                then.status(200)
                    .header("content-type", "application/json")
                    .header("Mcp-Session-Id", "mcp-sess-1")
                    .json_body(json!({
                        "jsonrpc": "2.0",
                        "id": 0,
                        "result": {"protocolVersion": "2025-03-26"}
                    }));
            })
            .await;
        // 600 KiB of stdout, well past the Java `OUTPUT_TRUNCATE_BYTES` budget.
        let exec_payload = serde_json::to_string(&json!({
            "exitCode": 0,
            "stdout": "x".repeat(600 * 1024),
            "stderr": "warn",
        }))
        .unwrap();
        server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/mcp")
                    .body_contains("\"method\":\"tools/call\"");
                then.status(200)
                    .header("content-type", "application/json")
                    .json_body(json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "content": [{"type": "text", "text": exec_payload}],
                            "isError": false
                        }
                    }));
            })
            .await;

        let backend = mock_backend(&server, None);
        let handle = RemoteSandboxHandle::new("sbx-1", BTreeMap::new());
        let output = backend
            .exec_in(&handle, "cat big", Some(DEFAULT_WORKSPACE_ROOT), 5)
            .await
            .unwrap();
        assert!(output.truncated);
        assert_eq!(output.stdout.len(), crate::remote::OUTPUT_TRUNCATE_BYTES);
        assert_eq!(output.stderr, "warn");
    }
}
