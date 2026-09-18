//! AgentRun (Alibaba Cloud) sandbox backend.
//!
//! Mirrors the Java `io.agentscope.extensions.sandbox.agentrun` package:
//!
//! * data-plane lifecycle (`POST/GET/DELETE /sandboxes`, readiness polling), and
//! * command execution through the AgentRun MCP `process_exec_cmd` tool.
//!
//! The MCP call is issued as a stateless JSON-RPC 2.0 `tools/call` HTTP request. The full MCP
//! `initialize` handshake / SSE session management is not implemented in Rust (recorded as a
//! P2 gap in the module tracker); servers that require a session id will reject the call.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::{Client, Response};
use serde_json::{json, Value};

use crate::error::{SandboxErrorCode, SandboxExtensionError, SandboxResult};
use crate::http::join_url;
use crate::remote::{
    RemoteBackend, RemoteExecOutput, RemoteSandboxClient, RemoteSandboxHandle,
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

/// Validates option invariants that do not require a live sandbox.
pub fn validate_options(options: &AgentRunSandboxClientOptions) -> SandboxResult<()> {
    let missing = |field: &str| {
        SandboxExtensionError::Configuration(format!(
            "AgentRun {field} is required (AgentRunSandboxClientOptions)"
        ))
    };
    if options.api_key.as_deref().map(str::trim).unwrap_or("").is_empty() {
        return Err(missing("API key"));
    }
    if options.template_name.as_deref().map(str::trim).unwrap_or("").is_empty() {
        return Err(missing("template name"));
    }
    if options.mcp_server_url.as_deref().map(str::trim).unwrap_or("").is_empty() {
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
        options.account_id.as_deref().filter(|v| !v.trim().is_empty()),
        options.region.as_deref().filter(|v| !v.trim().is_empty()),
    ) {
        (Some(account), Some(region)) => {
            Ok(format!("https://{account}.agentrun-data.{region}.aliyuncs.com"))
        }
        _ => Err(SandboxExtensionError::Configuration(
            "AgentRun requires accountId+region or an explicit dataPlaneBaseUrl".to_string(),
        )),
    }
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
        Self { exit_code: 0, stdout: String::new(), stderr: String::new() }
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
            let result = node.get("result").filter(|r| r.is_object()).unwrap_or(&node);
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
    AgentRunExecResult { exit_code: 0, stdout: trimmed.to_string(), stderr: String::new() }
}

/// Extracts the sandbox status from a `GET /sandboxes/{id}` response.
pub fn parse_sandbox_status(value: &Value) -> Option<String> {
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
}

impl AgentRunBackend {
    /// Builds a backend after validating options and resolving the data-plane URL.
    pub fn new(options: AgentRunSandboxClientOptions) -> SandboxResult<Self> {
        validate_options(&options)?;
        let base_url = resolve_data_plane_base_url(&options)?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(options.connect_timeout_seconds))
            .timeout(Duration::from_secs(options.read_timeout_seconds))
            .build()
            .map_err(|e| SandboxExtensionError::Transport(format!("failed to build client: {e}")))?;
        let retry = RetryPolicy::agentrun(options.max_retries);
        Ok(Self { options, client, retry, base_url })
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
                            format!("AgentRun sandbox entered terminal state {status}: {sandbox_id}"),
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
        let mut extra = BTreeMap::new();
        if let Some(template) = &self.options.template_name {
            extra.insert("template_name".to_string(), template.clone());
        }
        if let Some(mcp) = &self.options.mcp_server_url {
            extra.insert("mcp_server_url".to_string(), mcp.clone());
        }
        RemoteSandboxHandle::new(sandbox_id, extra)
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
        let url = join_url(&self.base_url, "/sandboxes");
        let body = build_create_sandbox_body(&self.options, session_id)?;
        self.request_json(reqwest::Method::POST, url, Some(body)).await?;
        self.wait_until_ready(session_id).await?;
        Ok(self.handle_for(session_id))
    }

    async fn connect_sandbox(&self, sandbox_id: &str) -> SandboxResult<RemoteSandboxHandle> {
        let url = join_url(&self.base_url, &format!("/sandboxes/{sandbox_id}"));
        self.request_json(reqwest::Method::GET, url, None).await?;
        Ok(self.handle_for(sandbox_id))
    }

    async fn kill_sandbox(&self, sandbox_id: &str) -> SandboxResult<()> {
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
        let url = self.mcp_url()?;
        let request = build_mcp_tool_call(1, TOOL_EXEC, command, cwd, timeout_seconds);
        let api_key = self.api_key()?;
        let client = self.client.clone();
        let retry = self.retry.clone();
        let sandbox_id = handle.id.clone();
        let response = with_retries(&retry, move || {
            let client = client.clone();
            let url = url.clone();
            let api_key = api_key.clone();
            let request = request.clone();
            let sandbox_id = sandbox_id.clone();
            async move {
                let value = client
                    .post(&url)
                    .header("X-API-Key", api_key)
                    .header("X-AgentRun-Sandbox-Id", sandbox_id)
                    .json(&request)
                    .send()
                    .await
                    .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
                Self::decode_json(value, &url).await
            }
        })
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
        Ok(RemoteExecOutput {
            exit_code: parsed.exit_code,
            stdout: parsed.stdout,
            stderr: parsed.stderr,
            truncated: false,
        })
    }
}

/// AgentRun sandbox client (harness [`agentscope_harness::agent::sandbox::SandboxClient`]).
pub type AgentRunSandboxClient = RemoteSandboxClient;

/// Creates the harness [`agentscope_harness::agent::sandbox::SandboxClient`] for AgentRun.
pub fn agentrun_client(
    options: AgentRunSandboxClientOptions,
) -> SandboxResult<AgentRunSandboxClient> {
    Ok(RemoteSandboxClient::new(Arc::new(AgentRunBackend::new(options)?)))
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
        assert_eq!(body["nasMountConfig"]["mountPoints"][0]["serverAddr"], "nas.local");
        assert_eq!(body["nasMountConfig"]["mountPoints"][0]["enableTLS"], true);
        assert_eq!(body["ossMountConfig"]["mountPoints"][0]["bucketName"], "bucket");
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

        let nested = parse_exec_payload("banner\n{\"result\":{\"exit_code\":3,\"output\":\"nested\"}}");
        assert_eq!(nested.exit_code, 3);
        assert_eq!(nested.stdout, "nested");

        let plain = parse_exec_payload("just text");
        assert_eq!(plain.exit_code, 0);
        assert_eq!(plain.stdout, "just text");
    }

    #[test]
    fn parse_sandbox_status_reads_status_or_state() {
        assert_eq!(parse_sandbox_status(&json!({"status": "READY"})).as_deref(), Some("READY"));
        assert_eq!(parse_sandbox_status(&json!({"state": "RUNNING"})).as_deref(), Some("RUNNING"));
        assert_eq!(parse_sandbox_status(&json!({})), None);
    }

    #[test]
    fn backend_construction_validates_options() {
        assert!(AgentRunBackend::new(AgentRunSandboxClientOptions::default()).is_err());
        assert!(AgentRunBackend::new(base_options()).is_ok());
    }
}
