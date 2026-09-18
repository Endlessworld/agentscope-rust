//! E2B cloud sandbox backend.
//!
//! Mirrors the Java `io.agentscope.extensions.sandbox.e2b` package:
//!
//! * platform lifecycle (`POST /sandboxes`, `POST /sandboxes/{id}/connect`,
//!   `DELETE /sandboxes/{id}`, `POST /sandboxes/{id}/snapshots`), and
//! * command execution through envd (`process.Process/Start` over Connect, JSON codec),
//!   including the 5-byte Connect frame envelope and base64 `stdout`/`stderr` events.
//!
//! The `PROTO` codec is not implemented (it needs the envd protobuf schema); selecting it
//! yields [`SandboxExtensionError::Unsupported`] rather than silently mis-encoding. See the
//! module tracker for this recorded gap.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use reqwest::{Client, Response};
use serde_json::{json, Value};

use crate::error::{SandboxErrorCode, SandboxExtensionError, SandboxResult};
use crate::http::join_url;
use crate::remote::{
    persist_tar, RemoteBackend, RemoteExecOutput, RemoteSandboxClient, RemoteSandboxHandle,
};
use crate::retry::{with_retries, RetryPolicy};
use crate::workspace::{shell_quote, short_hash};

/// Port envd listens on inside an E2B sandbox.
pub const ENVD_PORT: u16 = 49983;
/// Connect-Protocol framing: flag byte + 4-byte big-endian length.
pub const CONNECT_HEADER_LEN: usize = 5;

/// How workspace bytes are persisted for E2B sandboxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum E2bPersistenceMode {
    /// Tar archive bytes (default, compatible with other Harness snapshots).
    #[default]
    Tar,
    /// E2B native snapshot + [`E2bSnapshotRefs`] marker bytes in the snapshot stream.
    NativeSnapshot,
}

impl E2bPersistenceMode {
    /// Stable string discriminator used in state metadata.
    pub fn as_str(self) -> &'static str {
        match self {
            E2bPersistenceMode::Tar => "TAR",
            E2bPersistenceMode::NativeSnapshot => "NATIVE_SNAPSHOT",
        }
    }

    /// Parses the discriminator, defaulting to [`E2bPersistenceMode::Tar`].
    pub fn parse(value: &str) -> Self {
        match value {
            "NATIVE_SNAPSHOT" => E2bPersistenceMode::NativeSnapshot,
            _ => E2bPersistenceMode::Tar,
        }
    }
}

/// Connect codec used by the envd `process.Process/Start` endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum E2bCodec {
    /// Protobuf codec (not implemented in Rust; see module docs).
    #[default]
    Proto,
    /// JSON codec.
    Json,
}

impl E2bCodec {
    /// Stable string discriminator used in state metadata.
    pub fn as_str(self) -> &'static str {
        match self {
            E2bCodec::Proto => "PROTO",
            E2bCodec::Json => "JSON",
        }
    }

    /// Parses the discriminator, defaulting to [`E2bCodec::Proto`].
    pub fn parse(value: &str) -> Self {
        match value {
            "JSON" => E2bCodec::Json,
            _ => E2bCodec::Proto,
        }
    }
}

/// Options for the E2B sandbox client (mirrors `E2bSandboxClientOptions`).
#[derive(Debug, Clone)]
pub struct E2bSandboxClientOptions {
    /// E2B API key (`X-API-Key`).
    pub api_key: Option<String>,
    /// Platform API base URL.
    pub api_base_url: String,
    /// Sandbox domain used to derive the envd host.
    pub domain: String,
    /// E2B template id (or snapshot id when creating from a snapshot).
    pub template_id: String,
    /// Absolute workspace root inside the sandbox.
    pub workspace_root: String,
    /// Sandbox lifetime in seconds.
    pub sandbox_timeout_seconds: u64,
    /// Run user (used for envd basic auth).
    pub run_user: String,
    /// Persistence mode.
    pub persistence_mode: E2bPersistenceMode,
    /// envd Connect codec.
    pub codec: E2bCodec,
    /// TCP connect timeout in seconds.
    pub connect_timeout_seconds: u64,
    /// Read timeout in seconds.
    pub read_timeout_seconds: u64,
    /// Maximum retry attempts.
    pub max_retries: u32,
}

impl Default for E2bSandboxClientOptions {
    fn default() -> Self {
        Self {
            api_key: None,
            api_base_url: "https://api.e2b.app".to_string(),
            domain: "e2b.app".to_string(),
            template_id: "base".to_string(),
            workspace_root: "/home/user".to_string(),
            sandbox_timeout_seconds: 300,
            run_user: "user".to_string(),
            persistence_mode: E2bPersistenceMode::Tar,
            codec: E2bCodec::Proto,
            connect_timeout_seconds: 30,
            read_timeout_seconds: 120,
            max_retries: 3,
        }
    }
}

/// Cross-language marker for E2B native snapshot references, aligned with
/// openai-agents-python `E2B_SANDBOX_SNAPSHOT_V1`.
#[derive(Debug, Clone, Copy, Default)]
pub struct E2bSnapshotRefs;

impl E2bSnapshotRefs {
    /// Magic prefix identifying a native-snapshot marker archive.
    pub const MAGIC_PREFIX: &'static [u8] = b"E2B_SANDBOX_SNAPSHOT_V1\n";

    /// Encodes a snapshot id into marker bytes.
    pub fn encode_snapshot_id(snapshot_id: &str) -> Vec<u8> {
        let body = serde_json::to_vec(&json!({ "snapshot_id": snapshot_id }))
            .unwrap_or_else(|_| b"{\"snapshot_id\":\"\"}".to_vec());
        let mut out = Vec::with_capacity(Self::MAGIC_PREFIX.len() + body.len());
        out.extend_from_slice(Self::MAGIC_PREFIX);
        out.extend_from_slice(&body);
        out
    }

    /// Decodes a snapshot id from marker bytes, or `None` when the marker is absent.
    pub fn decode_snapshot_id_if_present(raw: &[u8]) -> Option<String> {
        if raw.len() <= Self::MAGIC_PREFIX.len() || !raw.starts_with(Self::MAGIC_PREFIX) {
            return None;
        }
        let body = &raw[Self::MAGIC_PREFIX.len()..];
        let value: Value = serde_json::from_slice(body).ok()?;
        value
            .get("snapshot_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
}

/// Builds the `POST /sandboxes` request body.
pub fn build_create_sandbox_body(template_id: &str, timeout_seconds: u64) -> Value {
    json!({ "templateID": template_id, "timeout": timeout_seconds })
}

/// Builds the `POST /sandboxes/{id}/connect` request body.
pub fn build_connect_sandbox_body(timeout_seconds: u64) -> Value {
    json!({ "timeout": timeout_seconds })
}

/// Extracts lifecycle fields from an E2B sandbox JSON object into stable snake_case keys.
pub fn parse_sandbox_fields(value: &Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(id) = value.get("sandboxID").and_then(Value::as_str) {
        out.insert("sandbox_id".to_string(), id.to_string());
    }
    if let Some(domain) = value.get("domain").and_then(Value::as_str) {
        out.insert("sandbox_domain".to_string(), domain.to_string());
    }
    if let Some(token) = value.get("envdAccessToken").and_then(Value::as_str) {
        out.insert("envd_access_token".to_string(), token.to_string());
    }
    if let Some(version) = value.get("envdVersion").and_then(Value::as_str) {
        out.insert("envd_version".to_string(), version.to_string());
    }
    out
}

/// Derives the envd base URL from a sandbox id and domain.
pub fn envd_host(sandbox_id: &str, domain: &str) -> String {
    let dom = if domain.trim().is_empty() { "e2b.app" } else { domain };
    format!("https://{ENVD_PORT}-{sandbox_id}.{dom}")
}

/// Builds the envd `process.Process/Start` JSON request payload (`ProcessConfig`).
pub fn build_envd_start_json_body(command: &str, cwd: Option<&str>) -> Value {
    let mut process = json!({
        "cmd": "/bin/bash",
        "args": ["-l", "-c", command],
    });
    if let Some(cwd) = cwd.filter(|c| !c.trim().is_empty()) {
        process["cwd"] = Value::String(cwd.to_string());
    }
    json!({ "process": process, "stdin": false })
}

/// Wraps a payload in a Connect unary/stream envelope (flag + big-endian length).
pub fn encode_connect_envelope(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(CONNECT_HEADER_LEN + payload.len());
    out.push(0x00);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Splits a Connect response body into data-frame payloads (skipping trailers/invalid frames).
pub fn decode_connect_frames(data: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut cursor = 0usize;
    while cursor + CONNECT_HEADER_LEN <= data.len() {
        let flags = data[cursor];
        let len = u32::from_be_bytes([
            data[cursor + 1],
            data[cursor + 2],
            data[cursor + 3],
            data[cursor + 4],
        ]) & 0x7fff_ffff;
        cursor += CONNECT_HEADER_LEN;
        let len = len as usize;
        if cursor + len > data.len() {
            break;
        }
        let payload = data[cursor..cursor + len].to_vec();
        cursor += len;
        if flags == 0x00 {
            frames.push(payload);
        }
    }
    frames
}

/// A decoded envd `process` event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvdEvent {
    /// Bytes appended to stdout this event.
    pub stdout: Vec<u8>,
    /// Bytes appended to stderr this event.
    pub stderr: Vec<u8>,
    /// Process exit code when this is an end event.
    pub exit_code: Option<i32>,
}

/// Parses one JSON-coded envd `StartResponse` frame.
pub fn parse_envd_event(payload: &[u8]) -> SandboxResult<EnvdEvent> {
    let value: Value = serde_json::from_slice(payload).map_err(|e| {
        SandboxExtensionError::Serialization(format!("invalid envd event JSON: {e}"))
    })?;
    let event = value.get("event").cloned().unwrap_or(Value::Null);
    let mut out = EnvdEvent::default();
    if let Some(data) = event.get("data") {
        out.stdout = decode_b64_field(data, "stdout");
        out.stderr = decode_b64_field(data, "stderr");
    }
    if let Some(end) = event.get("end") {
        out.exit_code = end.get("exitCode").and_then(Value::as_i64).map(|v| v as i32);
    }
    Ok(out)
}

fn decode_b64_field(parent: &Value, field: &str) -> Vec<u8> {
    parent
        .get(field)
        .and_then(Value::as_str)
        .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
        .unwrap_or_default()
}

/// Aggregated result of draining an envd process stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvdStreamResult {
    /// Combined stdout bytes.
    pub stdout: Vec<u8>,
    /// Combined stderr bytes.
    pub stderr: Vec<u8>,
    /// Exit code (error when the stream ended without an end event).
    pub exit_code: Option<i32>,
}

/// Drains a full Connect response body into an aggregated result.
pub fn drain_envd_stream(data: &[u8]) -> SandboxResult<EnvdStreamResult> {
    let mut result = EnvdStreamResult::default();
    for frame in decode_connect_frames(data) {
        if let Ok(event) = parse_envd_event(&frame) {
            result.stdout.extend_from_slice(&event.stdout);
            result.stderr.extend_from_slice(&event.stderr);
            if let Some(code) = event.exit_code {
                result.exit_code = Some(code);
            }
        }
    }
    Ok(result)
}

/// E2B [`RemoteBackend`] implementation.
#[derive(Debug)]
pub struct E2bBackend {
    options: E2bSandboxClientOptions,
    client: Client,
    retry: RetryPolicy,
}

impl E2bBackend {
    /// Builds a backend, wiring up the reqwest client and retry policy.
    pub fn new(options: E2bSandboxClientOptions) -> SandboxResult<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(options.connect_timeout_seconds))
            .timeout(Duration::from_secs(options.read_timeout_seconds))
            .build()
            .map_err(|e| SandboxExtensionError::Transport(format!("failed to build client: {e}")))?;
        let retry = RetryPolicy::e2b(options.max_retries);
        Ok(Self { options, client, retry })
    }

    /// Returns the configured options.
    pub fn options(&self) -> &E2bSandboxClientOptions {
        &self.options
    }

    fn api_key(&self) -> SandboxResult<String> {
        self.options
            .api_key
            .clone()
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| {
                SandboxExtensionError::Configuration(
                    "E2B API key is required (E2bSandboxClientOptions::api_key)".to_string(),
                )
            })
    }

    fn domain_from(&self, handle: &RemoteSandboxHandle) -> String {
        handle
            .extra
            .get("sandbox_domain")
            .cloned()
            .filter(|d| !d.trim().is_empty())
            .unwrap_or_else(|| self.options.domain.clone())
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

    async fn platform_post(&self, path: &str, body: &Value) -> SandboxResult<Value> {
        let url = join_url(&self.options.api_base_url, path);
        let api_key = self.api_key()?;
        let client = self.client.clone();
        let retry = self.retry.clone();
        with_retries(&retry, move || {
            let client = client.clone();
            let url = url.clone();
            let api_key = api_key.clone();
            let body = body.clone();
            async move {
                let response = client
                    .post(&url)
                    .header("X-API-Key", api_key)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
                Self::decode_json(response, &url).await
            }
        })
        .await
    }

    async fn create_from_template(&self, template_id: &str) -> SandboxResult<BTreeMap<String, String>> {
        let body = build_create_sandbox_body(template_id, self.options.sandbox_timeout_seconds);
        let value = self.platform_post("/sandboxes", &body).await?;
        Ok(parse_sandbox_fields(&value))
    }
}

#[async_trait::async_trait]
impl RemoteBackend for E2bBackend {
    fn backend_name(&self) -> &'static str {
        "e2b"
    }

    fn endpoint_base(&self) -> String {
        self.options.api_base_url.clone()
    }

    fn workspace_root(&self) -> &str {
        &self.options.workspace_root
    }

    async fn create_sandbox(&self, _session_id: &str) -> SandboxResult<RemoteSandboxHandle> {
        let mut fields = self.create_from_template(&self.options.template_id).await?;
        let id = fields
            .get("sandbox_id")
            .cloned()
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| {
                SandboxExtensionError::Runtime(
                    SandboxErrorCode::WorkspaceStartError,
                    "E2B create sandbox response missing sandboxID".to_string(),
                )
            })?;
        fields.insert("template_id".to_string(), self.options.template_id.clone());
        fields.insert("persistence_mode".to_string(), self.options.persistence_mode.as_str().to_string());
        fields.insert("codec".to_string(), self.options.codec.as_str().to_string());
        fields.insert("workspace_projection_hash".to_string(), String::new());
        Ok(RemoteSandboxHandle::new(id, fields))
    }

    async fn connect_sandbox(&self, sandbox_id: &str) -> SandboxResult<RemoteSandboxHandle> {
        let body = build_connect_sandbox_body(self.options.sandbox_timeout_seconds);
        let path = format!("/sandboxes/{sandbox_id}/connect");
        let value = self.platform_post(&path, &body).await?;
        let mut fields = parse_sandbox_fields(&value);
        fields
            .entry("sandbox_id".to_string())
            .or_insert_with(|| sandbox_id.to_string());
        fields.insert("template_id".to_string(), self.options.template_id.clone());
        fields.insert("persistence_mode".to_string(), self.options.persistence_mode.as_str().to_string());
        fields.insert("codec".to_string(), self.options.codec.as_str().to_string());
        Ok(RemoteSandboxHandle::new(sandbox_id.to_string(), fields))
    }

    async fn kill_sandbox(&self, sandbox_id: &str) -> SandboxResult<()> {
        let url = join_url(&self.options.api_base_url, &format!("/sandboxes/{sandbox_id}"));
        let api_key = self.api_key()?;
        let client = self.client.clone();
        let retry = self.retry.clone();
        with_retries(&retry, move || {
            let client = client.clone();
            let url = url.clone();
            let api_key = api_key.clone();
            async move {
                let response = client
                    .delete(&url)
                    .header("X-API-Key", api_key)
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
        if self.options.codec != E2bCodec::Json {
            return Err(SandboxExtensionError::Unsupported(
                "E2B envd PROTO codec is not implemented; set codec = E2bCodec::Json".to_string(),
            ));
        }
        let host = envd_host(&handle.id, &self.domain_from(handle));
        let url = join_url(&host, "process.Process/Start");
        let payload = serde_json::to_vec(&build_envd_start_json_body(command, cwd))
            .map_err(|e| SandboxExtensionError::Serialization(e.to_string()))?;
        let envelope = encode_connect_envelope(&payload);
        let auth = basic_auth_header(&self.options.run_user);
        let access_token = handle.extra.get("envd_access_token").cloned();
        let sandbox_id = handle.id.clone();
        let client = self.client.clone();
        let retry = self.retry.clone();
        let body = with_retries(&retry, move || {
            let client = client.clone();
            let url = url.clone();
            let auth = auth.clone();
            let access_token = access_token.clone();
            let sandbox_id = sandbox_id.clone();
            let envelope = envelope.clone();
            async move {
                let mut request = client
                    .post(&url)
                    .header("Content-Type", "application/connect+json")
                    .header("Connect-Protocol-Version", "1")
                    .header("User-Agent", "agentscope-rust-e2b")
                    .header("E2b-Sandbox-Id", sandbox_id)
                    .header("E2b-Sandbox-Port", ENVD_PORT.to_string())
                    .header("Authorization", auth)
                    .body(envelope);
                if let Some(token) = &access_token {
                    request = request.header("X-Access-Token", token);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
                let status = response.status();
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
                if !status.is_success() {
                    return Err(SandboxExtensionError::http(
                        status.as_u16(),
                        String::from_utf8_lossy(&bytes).into_owned(),
                    ));
                }
                Ok(bytes.to_vec())
            }
        })
        .await?;
        let _ = timeout_seconds;
        let stream = drain_envd_stream(&body)?;
        let exit_code = stream.exit_code.ok_or_else(|| {
            SandboxExtensionError::Runtime(
                SandboxErrorCode::CommandExecutionError,
                "envd process stream ended before receiving an exit code".to_string(),
            )
        })?;
        Ok(RemoteExecOutput {
            exit_code,
            stdout: String::from_utf8_lossy(&stream.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stream.stderr).into_owned(),
            truncated: false,
        })
    }

    async fn persist_workspace_bytes(
        &self,
        handle: &RemoteSandboxHandle,
        root: &str,
    ) -> SandboxResult<Vec<u8>> {
        let mode = handle
            .extra
            .get("persistence_mode")
            .map(|v| E2bPersistenceMode::parse(v))
            .unwrap_or(self.options.persistence_mode);
        if mode == E2bPersistenceMode::NativeSnapshot {
            let path = format!("/sandboxes/{}/snapshots", handle.id);
            let value = self.platform_post(&path, &json!({})).await?;
            let snapshot_id = value
                .get("snapshotID")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    SandboxExtensionError::Runtime(
                        SandboxErrorCode::WorkspaceStopError,
                        format!("E2B snapshot response missing snapshotID: {value}"),
                    )
                })?;
            return Ok(E2bSnapshotRefs::encode_snapshot_id(snapshot_id));
        }
        persist_tar(self, handle, root).await
    }

    async fn hydrate_workspace_bytes(
        &self,
        handle: &RemoteSandboxHandle,
        root: &str,
        archive: &[u8],
    ) -> SandboxResult<Option<RemoteSandboxHandle>> {
        if let Some(snapshot_id) = E2bSnapshotRefs::decode_snapshot_id_if_present(archive) {
            if !snapshot_id.trim().is_empty() {
                let mut fields = self.create_from_template(&snapshot_id).await?;
                let id = fields
                    .get("sandbox_id")
                    .cloned()
                    .ok_or_else(|| {
                        SandboxExtensionError::Runtime(
                            SandboxErrorCode::WorkspaceStartError,
                            "E2B snapshot restore response missing sandboxID".to_string(),
                        )
                    })?;
                fields.insert("template_id".to_string(), snapshot_id);
                fields.insert(
                    "persistence_mode".to_string(),
                    E2bPersistenceMode::NativeSnapshot.as_str().to_string(),
                );
                fields.insert("codec".to_string(), self.options.codec.as_str().to_string());
                return Ok(Some(RemoteSandboxHandle::new(id, fields)));
            }
        }
        crate::remote::hydrate_tar(self, handle, root, archive).await?;
        Ok(None)
    }
}

/// Builds the envd `Authorization: Basic` header for the run user.
pub fn basic_auth_header(run_user: &str) -> String {
    let user = if run_user.trim().is_empty() { "user" } else { run_user };
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:"));
    format!("Basic {encoded}")
}

/// E2B sandbox client (harness [`agentscope_harness::agent::sandbox::SandboxClient`]).
pub type E2bSandboxClient = RemoteSandboxClient;

/// Creates the harness [`agentscope_harness::agent::sandbox::SandboxClient`] for E2B.
pub fn e2b_client(options: E2bSandboxClientOptions) -> SandboxResult<E2bSandboxClient> {
    Ok(RemoteSandboxClient::new(Arc::new(E2bBackend::new(options)?)))
}

/// Builds the shell command that materializes the workspace on E2B (used by tests/docs).
pub fn build_remote_workspace_mkdir(root: &str) -> String {
    format!("mkdir -p {}", shell_quote(root))
}

/// Stable hash helper re-exported for snapshot-marker tests.
pub fn marker_hash(snapshot_id: &str) -> u64 {
    short_hash(snapshot_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::RemoteSandboxStateData;

    #[test]
    fn create_body_matches_java() {
        let body = build_create_sandbox_body("base", 300);
        assert_eq!(body["templateID"], "base");
        assert_eq!(body["timeout"], 300);
    }

    #[test]
    fn connect_body_only_carries_timeout() {
        assert_eq!(build_connect_sandbox_body(120), json!({ "timeout": 120 }));
    }

    #[test]
    fn parse_sandbox_fields_extracts_lifecycle() {
        let value = json!({
            "sandboxID": "sbx-1",
            "domain": "e2b.dev",
            "envdAccessToken": "tok",
            "envdVersion": "0.1.5"
        });
        let fields = parse_sandbox_fields(&value);
        assert_eq!(fields.get("sandbox_id").map(String::as_str), Some("sbx-1"));
        assert_eq!(fields.get("sandbox_domain").map(String::as_str), Some("e2b.dev"));
        assert_eq!(fields.get("envd_access_token").map(String::as_str), Some("tok"));
        assert_eq!(fields.get("envd_version").map(String::as_str), Some("0.1.5"));
    }

    #[test]
    fn envd_host_uses_port_and_domain() {
        assert_eq!(envd_host("sbx-1", "e2b.app"), "https://49983-sbx-1.e2b.app");
        assert_eq!(envd_host("sbx-1", ""), "https://49983-sbx-1.e2b.app");
    }

    #[test]
    fn envd_start_body_shape() {
        let body = build_envd_start_json_body("echo hi", Some("/home/user"));
        assert_eq!(body["process"]["cmd"], "/bin/bash");
        assert_eq!(body["process"]["args"], json!(["-l", "-c", "echo hi"]));
        assert_eq!(body["process"]["cwd"], "/home/user");
        assert_eq!(body["stdin"], false);
        let no_cwd = build_envd_start_json_body("ls", None);
        assert!(no_cwd["process"].get("cwd").is_none());
    }

    #[test]
    fn connect_envelope_round_trips() {
        let payload = br#"{"hello":"world"}"#;
        let envelope = encode_connect_envelope(payload);
        assert_eq!(envelope[0], 0x00);
        assert_eq!(
            u32::from_be_bytes([envelope[1], envelope[2], envelope[3], envelope[4]]) as usize,
            payload.len()
        );
        let frames = decode_connect_frames(&envelope);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], payload);
    }

    #[test]
    fn decode_frames_skips_trailers_and_truncated() {
        let mut data = encode_connect_envelope(b"a");
        data.extend_from_slice(&[0x02, 0, 0, 0, 1, b'x']); // trailer flag -> skipped
        data.extend_from_slice(&[0x00, 0, 0, 0, 9]); // truncated -> stop
        let frames = decode_connect_frames(&data);
        assert_eq!(frames, vec![b"a".to_vec()]);
    }

    #[test]
    fn drain_envd_stream_aggregates_events() {
        let stdout_b64 = base64::engine::general_purpose::STANDARD.encode(b"hello ");
        let stderr_b64 = base64::engine::general_purpose::STANDARD.encode(b"warn");
        let mut body = Vec::new();
        body.extend_from_slice(&encode_connect_envelope(
            serde_json::to_vec(&json!({"event":{"data":{"stdout":stdout_b64}}})).unwrap().as_slice(),
        ));
        body.extend_from_slice(&encode_connect_envelope(
            serde_json::to_vec(&json!({"event":{"data":{"stdout":base64::engine::general_purpose::STANDARD.encode(b"world"),"stderr":stderr_b64}}})).unwrap().as_slice(),
        ));
        body.extend_from_slice(&encode_connect_envelope(
            serde_json::to_vec(&json!({"event":{"end":{"exitCode":0}}})).unwrap().as_slice(),
        ));
        let result = drain_envd_stream(&body).unwrap();
        assert_eq!(result.stdout, b"hello world");
        assert_eq!(result.stderr, b"warn");
        assert_eq!(result.exit_code, Some(0));
    }

    #[test]
    fn snapshot_marker_round_trips() {
        let encoded = E2bSnapshotRefs::encode_snapshot_id("snap-42");
        assert!(encoded.starts_with(E2bSnapshotRefs::MAGIC_PREFIX));
        assert_eq!(
            E2bSnapshotRefs::decode_snapshot_id_if_present(&encoded).as_deref(),
            Some("snap-42")
        );
        assert_eq!(E2bSnapshotRefs::decode_snapshot_id_if_present(b"plain tar bytes"), None);
        assert_eq!(E2bSnapshotRefs::decode_snapshot_id_if_present(b"E2B_SANDBOX_SNAPSHOT_V1\n"), None);
    }

    #[test]
    fn persistence_mode_and_codec_parse() {
        assert_eq!(E2bPersistenceMode::parse("NATIVE_SNAPSHOT"), E2bPersistenceMode::NativeSnapshot);
        assert_eq!(E2bPersistenceMode::parse("bogus"), E2bPersistenceMode::Tar);
        assert_eq!(E2bCodec::parse("JSON"), E2bCodec::Json);
        assert_eq!(E2bCodec::parse("bogus"), E2bCodec::Proto);
    }

    #[test]
    fn basic_auth_header_encodes_user() {
        assert_eq!(basic_auth_header("user"), "Basic dXNlcjo=");
        assert_eq!(basic_auth_header(""), "Basic dXNlcjo=");
    }

    #[test]
    fn options_defaults_match_java() {
        let options = E2bSandboxClientOptions::default();
        assert_eq!(options.api_base_url, "https://api.e2b.app");
        assert_eq!(options.domain, "e2b.app");
        assert_eq!(options.template_id, "base");
        assert_eq!(options.workspace_root, "/home/user");
        assert_eq!(options.sandbox_timeout_seconds, 300);
        assert_eq!(options.run_user, "user");
    }

    #[tokio::test]
    async fn proto_codec_exec_is_unsupported() {
        let backend = E2bBackend::new(E2bSandboxClientOptions {
            api_key: Some("k".to_string()),
            ..Default::default()
        })
        .unwrap();
        let handle = RemoteSandboxHandle::new("sbx", BTreeMap::new());
        let err = backend.exec_in(&handle, "echo hi", None, 5).await.unwrap_err();
        assert!(matches!(err, SandboxExtensionError::Unsupported(_)));
    }

    #[tokio::test]
    async fn create_sandbox_requires_api_key() {
        let backend = E2bBackend::new(E2bSandboxClientOptions::default()).unwrap();
        let err = backend.create_sandbox("sess").await.unwrap_err();
        assert!(matches!(err, SandboxExtensionError::Configuration(_)));
    }

    #[tokio::test]
    async fn platform_create_against_mock_server() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let mock = server.mock_async(|when, then| {
            when.method(POST)
                .path("/sandboxes")
                .header("x-api-key", "secret")
                .json_body(json!({"templateID":"base","timeout":300}));
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"sandboxID":"sbx-9","domain":"e2b.dev","envdAccessToken":"tok"}"#);
        }).await;
        let backend = E2bBackend::new(E2bSandboxClientOptions {
            api_key: Some("secret".to_string()),
            api_base_url: server.base_url(),
            ..Default::default()
        })
        .unwrap();
        let handle = backend.create_sandbox("sess-1").await.unwrap();
        mock.assert_async().await;
        assert_eq!(handle.id, "sbx-9");
        assert_eq!(handle.extra.get("sandbox_domain").map(String::as_str), Some("e2b.dev"));
        assert_eq!(handle.extra.get("persistence_mode").map(String::as_str), Some("TAR"));
        // Sanity: state projection carries the backend discriminator.
        let data = RemoteSandboxStateData {
            session_id: "sess-1".to_string(),
            backend: backend.backend_name().to_string(),
            sandbox_id: Some(handle.id.clone()),
            endpoint: backend.endpoint_base(),
            token: None,
            workspace_root: backend.workspace_root().to_string(),
            sandbox_owned: true,
            workspace_root_ready: false,
            workspace_projection_hash: None,
            extra: handle.extra.clone(),
            workspace_spec: None,
        };
        assert_eq!(data.to_harness_state().session_id(), "sess-1");
    }
}
