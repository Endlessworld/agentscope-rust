//! Daytona cloud sandbox backend.
//!
//! Mirrors the Java `io.agentscope.extensions.sandbox.daytona` package: control-plane lifecycle
//! (`POST /sandbox`, `POST /sandbox/{id}/start`, `GET /sandbox/{id}`, `DELETE /sandbox/{id}`)
//! and command execution through the toolbox (`POST /toolbox/{id}/process/execute`).
//!
//! Workspace persistence reuses the shared `tar | base64` path from [`crate::remote`].

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

/// Default image used when no snapshot is supplied.
pub const DEFAULT_IMAGE: &str = "ubuntu:22.04";
/// Default workspace root inside a Daytona sandbox.
pub const DEFAULT_WORKSPACE_ROOT: &str = "/home/daytona/workspace";
/// Poll interval while waiting for a sandbox to start, in milliseconds.
pub const START_POLL_INTERVAL_MS: u64 = 1500;

/// Options for the Daytona sandbox client (mirrors `DaytonaSandboxClientOptions`).
#[derive(Debug, Clone)]
pub struct DaytonaSandboxClientOptions {
    /// Daytona API key (`Authorization: Bearer`).
    pub api_key: Option<String>,
    /// Control-plane base URL.
    pub control_plane_base_url: String,
    /// Toolbox (process API) base URL.
    pub toolbox_base_url: String,
    /// Image used when no snapshot is configured.
    pub image: String,
    /// Optional snapshot id; when set, `image` is ignored.
    pub snapshot_id: Option<String>,
    /// vCPU count.
    pub cpu: Option<u32>,
    /// Memory in GiB.
    pub memory: Option<u32>,
    /// Disk in GiB.
    pub disk: Option<u32>,
    /// Absolute workspace root inside the sandbox.
    pub workspace_root: String,
    /// TCP connect timeout in seconds.
    pub connect_timeout_seconds: u64,
    /// Read timeout in seconds.
    pub read_timeout_seconds: u64,
    /// Maximum retry attempts.
    pub max_retries: u32,
    /// Max seconds to wait for the sandbox to start.
    pub start_timeout_seconds: u64,
}

impl Default for DaytonaSandboxClientOptions {
    fn default() -> Self {
        Self {
            api_key: None,
            control_plane_base_url: "https://app.daytona.io/api".to_string(),
            toolbox_base_url: "https://proxy.app.daytona.io".to_string(),
            image: DEFAULT_IMAGE.to_string(),
            snapshot_id: None,
            cpu: Some(1),
            memory: Some(1),
            disk: Some(3),
            workspace_root: DEFAULT_WORKSPACE_ROOT.to_string(),
            connect_timeout_seconds: 30,
            read_timeout_seconds: 120,
            max_retries: 3,
            start_timeout_seconds: 300,
        }
    }
}

/// Builds the `POST /sandbox` request body.
pub fn build_create_sandbox_body(options: &DaytonaSandboxClientOptions) -> Value {
    let mut body = serde_json::Map::new();
    match options.snapshot_id.as_ref().filter(|s| !s.trim().is_empty()) {
        Some(snapshot) => {
            body.insert("snapshot".to_string(), Value::String(snapshot.clone()));
        }
        None => {
            body.insert("image".to_string(), Value::String(options.image.clone()));
        }
    }
    if let Some(cpu) = options.cpu {
        body.insert("cpu".to_string(), json!(cpu));
    }
    if let Some(memory) = options.memory {
        body.insert("memory".to_string(), json!(memory));
    }
    if let Some(disk) = options.disk {
        body.insert("disk".to_string(), json!(disk));
    }
    Value::Object(body)
}

/// Builds the toolbox `process/execute` body.
pub fn build_execute_body(command: &str, cwd: Option<&str>, timeout_seconds: u64) -> Value {
    let mut body = json!({
        "command": command,
        "timeout": timeout_seconds.max(1),
    });
    if let Some(cwd) = cwd.filter(|c| !c.trim().is_empty()) {
        body["cwd"] = Value::String(cwd.to_string());
    }
    body
}

/// Extracts the sandbox id from a create response (`id` or `sandboxId`).
pub fn parse_create_sandbox_id(value: &Value) -> Option<String> {
    value
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| value.get("sandboxId").and_then(Value::as_str))
        .filter(|id| !id.trim().is_empty())
        .map(|id| id.to_string())
}

/// Extracts the textual state from a `GET /sandbox/{id}` response.
pub fn parse_sandbox_state(value: &Value) -> Option<String> {
    value
        .get("state")
        .and_then(Value::as_str)
        .or_else(|| value.get("status").and_then(Value::as_str))
        .map(|s| s.to_string())
}

/// Result of a toolbox command execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaytonaExecResult {
    /// Exit code (defaults to -1 when absent).
    pub exit_code: i32,
    /// Combined output text.
    pub result: String,
}

/// Parses a toolbox `process/execute` response.
pub fn parse_execute_result(value: &Value) -> DaytonaExecResult {
    DaytonaExecResult {
        exit_code: value.get("exitCode").and_then(Value::as_i64).unwrap_or(-1) as i32,
        result: value
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

/// Strips the leading slash to form a toolbox-relative cwd (mirrors Java `relativeCwd`).
pub fn relative_cwd(absolute: &str) -> String {
    let trimmed = absolute.trim();
    trimmed.strip_prefix('/').unwrap_or(trimmed).to_string()
}

/// Daytona [`RemoteBackend`] implementation.
#[derive(Debug)]
pub struct DaytonaBackend {
    options: DaytonaSandboxClientOptions,
    client: Client,
    retry: RetryPolicy,
}

impl DaytonaBackend {
    /// Builds a backend, wiring up the reqwest client and retry policy.
    pub fn new(options: DaytonaSandboxClientOptions) -> SandboxResult<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(options.connect_timeout_seconds))
            .timeout(Duration::from_secs(options.read_timeout_seconds))
            .build()
            .map_err(|e| SandboxExtensionError::Transport(format!("failed to build client: {e}")))?;
        let retry = RetryPolicy::daytona(options.max_retries);
        Ok(Self { options, client, retry })
    }

    /// Returns the configured options.
    pub fn options(&self) -> &DaytonaSandboxClientOptions {
        &self.options
    }

    fn bearer(&self) -> SandboxResult<String> {
        self.options
            .api_key
            .clone()
            .filter(|k| !k.trim().is_empty())
            .map(|k| format!("Bearer {k}"))
            .ok_or_else(|| {
                SandboxExtensionError::Configuration(
                    "Daytona API key is required (DaytonaSandboxClientOptions::api_key)"
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

    async fn request_json(&self, method: reqwest::Method, url: String, body: Option<Value>) -> SandboxResult<Value> {
        let bearer = self.bearer()?;
        let client = self.client.clone();
        let retry = self.retry.clone();
        with_retries(&retry, move || {
            let client = client.clone();
            let url = url.clone();
            let method = method.clone();
            let bearer = bearer.clone();
            let body = body.clone();
            async move {
                let mut request = client.request(method, &url).header("Authorization", bearer);
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

    async fn execute_raw(
        &self,
        sandbox_id: &str,
        command: &str,
        cwd: Option<&str>,
        timeout_seconds: u64,
    ) -> SandboxResult<DaytonaExecResult> {
        let url = join_url(
            &self.options.toolbox_base_url,
            &format!("/toolbox/{sandbox_id}/process/execute"),
        );
        let body = build_execute_body(command, cwd, timeout_seconds);
        let value = self.request_json(reqwest::Method::POST, url, Some(body)).await?;
        Ok(parse_execute_result(&value))
    }

    async fn wait_until_started(&self, sandbox_id: &str) -> SandboxResult<()> {
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(self.options.start_timeout_seconds.max(1));
        loop {
            let url = join_url(
                &self.options.control_plane_base_url,
                &format!("/sandbox/{sandbox_id}"),
            );
            if let Ok(value) = self.request_json(reqwest::Method::GET, url, None).await {
                if let Some(state) = parse_sandbox_state(&value) {
                    if state.to_lowercase().contains("start") {
                        return Ok(());
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(SandboxExtensionError::Runtime(
                    SandboxErrorCode::WorkspaceStartError,
                    format!("Daytona sandbox did not become ready in time: {sandbox_id}"),
                ));
            }
            tokio::time::sleep(Duration::from_millis(START_POLL_INTERVAL_MS)).await;
        }
    }

    async fn start_sandbox(&self, sandbox_id: &str) -> SandboxResult<()> {
        let url = join_url(
            &self.options.control_plane_base_url,
            &format!("/sandbox/{sandbox_id}/start"),
        );
        self.request_json(reqwest::Method::POST, url, Some(json!({}))).await?;
        Ok(())
    }

    fn handle_for(&self, sandbox_id: &str) -> RemoteSandboxHandle {
        let mut extra = BTreeMap::new();
        extra.insert(
            "image".to_string(),
            self.options.image.clone(),
        );
        if let Some(snapshot) = &self.options.snapshot_id {
            extra.insert("snapshot_id".to_string(), snapshot.clone());
        }
        RemoteSandboxHandle::new(sandbox_id, extra)
    }
}

#[async_trait::async_trait]
impl RemoteBackend for DaytonaBackend {
    fn backend_name(&self) -> &'static str {
        "daytona"
    }

    fn endpoint_base(&self) -> String {
        self.options.control_plane_base_url.clone()
    }

    fn workspace_root(&self) -> &str {
        &self.options.workspace_root
    }

    async fn create_sandbox(&self, _session_id: &str) -> SandboxResult<RemoteSandboxHandle> {
        let url = join_url(&self.options.control_plane_base_url, "/sandbox");
        let body = build_create_sandbox_body(&self.options);
        let value = self.request_json(reqwest::Method::POST, url, Some(body)).await?;
        let id = parse_create_sandbox_id(&value).ok_or_else(|| {
            SandboxExtensionError::Runtime(
                SandboxErrorCode::WorkspaceStartError,
                format!("Daytona create sandbox response missing id: {value}"),
            )
        })?;
        self.start_sandbox(&id).await?;
        self.wait_until_started(&id).await?;
        Ok(self.handle_for(&id))
    }

    async fn connect_sandbox(&self, sandbox_id: &str) -> SandboxResult<RemoteSandboxHandle> {
        let url = join_url(
            &self.options.control_plane_base_url,
            &format!("/sandbox/{sandbox_id}"),
        );
        self.request_json(reqwest::Method::GET, url, None).await?;
        self.start_sandbox(sandbox_id).await?;
        self.wait_until_started(sandbox_id).await?;
        Ok(self.handle_for(sandbox_id))
    }

    async fn kill_sandbox(&self, sandbox_id: &str) -> SandboxResult<()> {
        let url = join_url(
            &self.options.control_plane_base_url,
            &format!("/sandbox/{sandbox_id}"),
        );
        let bearer = self.bearer()?;
        let client = self.client.clone();
        let retry = self.retry.clone();
        with_retries(&retry, move || {
            let client = client.clone();
            let url = url.clone();
            let bearer = bearer.clone();
            async move {
                let response = client
                    .delete(&url)
                    .header("Authorization", bearer)
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
        let cwd = cwd
            .map(|c| c.to_string())
            .unwrap_or_else(|| relative_cwd(&self.options.workspace_root));
        let result = self
            .execute_raw(&handle.id, command, Some(&cwd), timeout_seconds)
            .await?;
        Ok(RemoteExecOutput {
            exit_code: result.exit_code,
            stdout: result.result,
            stderr: String::new(),
            truncated: false,
        })
    }
}

/// Daytona sandbox client (harness [`agentscope_harness::agent::sandbox::SandboxClient`]).
pub type DaytonaSandboxClient = RemoteSandboxClient;

/// Creates the harness [`agentscope_harness::agent::sandbox::SandboxClient`] for Daytona.
pub fn daytona_client(options: DaytonaSandboxClientOptions) -> SandboxResult<DaytonaSandboxClient> {
    Ok(RemoteSandboxClient::new(Arc::new(DaytonaBackend::new(options)?)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn create_body_prefers_snapshot_over_image() {
        let mut options = DaytonaSandboxClientOptions::default();
        let body = build_create_sandbox_body(&options);
        assert_eq!(body["image"], DEFAULT_IMAGE);
        assert!(body.get("snapshot").is_none());
        assert_eq!(body["cpu"], 1);
        assert_eq!(body["memory"], 1);
        assert_eq!(body["disk"], 3);

        options.snapshot_id = Some("snap-1".to_string());
        let body = build_create_sandbox_body(&options);
        assert_eq!(body["snapshot"], "snap-1");
        assert!(body.get("image").is_none());
    }

    #[test]
    fn create_body_omits_absent_resources() {
        let options = DaytonaSandboxClientOptions {
            cpu: None,
            memory: None,
            disk: None,
            ..Default::default()
        };
        let body = build_create_sandbox_body(&options);
        assert!(body.get("cpu").is_none());
        assert!(body.get("memory").is_none());
        assert!(body.get("disk").is_none());
    }

    #[test]
    fn execute_body_has_command_and_timeout() {
        let body = build_execute_body("ls", Some("home/daytona"), 0);
        assert_eq!(body["command"], "ls");
        assert_eq!(body["cwd"], "home/daytona");
        assert_eq!(body["timeout"], 1);
        let no_cwd = build_execute_body("ls", None, 30);
        assert!(no_cwd.get("cwd").is_none());
    }

    #[test]
    fn parse_helpers() {
        assert_eq!(parse_create_sandbox_id(&json!({"id": "a"})).as_deref(), Some("a"));
        assert_eq!(parse_create_sandbox_id(&json!({"sandboxId": "b"})).as_deref(), Some("b"));
        assert_eq!(parse_create_sandbox_id(&json!({"id": ""})), None);
        assert_eq!(parse_sandbox_state(&json!({"state": "started"})).as_deref(), Some("started"));
        assert_eq!(parse_sandbox_state(&json!({"status": "STARTING"})).as_deref(), Some("STARTING"));
        let result = parse_execute_result(&json!({"exitCode": 0, "result": "ok"}));
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.result, "ok");
        assert_eq!(parse_execute_result(&json!({})).exit_code, -1);
    }

    #[test]
    fn relative_cwd_strips_leading_slash() {
        assert_eq!(relative_cwd("/home/daytona/workspace"), "home/daytona/workspace");
        assert_eq!(relative_cwd("home"), "home");
        assert_eq!(relative_cwd(""), "");
    }

    #[test]
    fn base64_tar_round_trip_via_toolbox_shape() {
        // Ensures the default persistence encoding path is valid base64 for Daytona output.
        let raw = b"tar-bytes";
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        let cleaned: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(
            base64::engine::general_purpose::STANDARD.decode(cleaned).unwrap(),
            raw
        );
    }

    #[tokio::test]
    async fn create_requires_api_key() {
        let backend = DaytonaBackend::new(DaytonaSandboxClientOptions::default()).unwrap();
        assert!(matches!(
            backend.create_sandbox("s").await.unwrap_err(),
            SandboxExtensionError::Configuration(_)
        ));
    }
}
