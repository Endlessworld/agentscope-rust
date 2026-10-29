//! E2B cloud sandbox backend.
//!
//! Mirrors the Java `io.agentscope.extensions.sandbox.e2b` package:
//!
//! * platform lifecycle (`POST /sandboxes`, `POST /sandboxes/{id}/connect`,
//!   `DELETE /sandboxes/{id}`, `POST /sandboxes/{id}/snapshots`),
//! * snapshot maintenance (`DELETE /templates/{snapshotId}`) plus the opt-in
//!   [`E2bSandboxClientOptions::snapshot_retention`] window described on
//!   [`E2bBackend::prune_known_snapshots`] — note that E2B has **no snapshot listing endpoint**,
//!   so retention only ever sees the ids this process recorded,
//! * volume mounts attached at creation time (`volumeMounts: [{name, path}]` on
//!   `POST /sandboxes`, mirroring the Java `E2bVolumeMount`),
//! * command execution through envd (`process.Process/Start` over Connect, JSON codec),
//!   including the 5-byte Connect frame envelope and base64 `stdout`/`stderr` events, and
//! * a reliable exec deadline: the Connect `Connect-Timeout-Ms` header makes envd kill the
//!   remote process server-side, while the client keeps a slightly later backup deadline
//!   (Java `E2bEnvdProcessClient`, commit `aa66abfd0`), and
//! * envd file transfer through the Filesystem REST API (`POST`/`GET /files`) — the prerequisite
//!   for the workspace-projection work; the envd data-plane base URL can be overridden with the
//!   [`E2B_SANDBOX_URL_ENV`] environment variable (the official SDK's `E2B_SANDBOX_URL`).
//!
//! The default codec is [`E2bCodec::Json`], which connectrpc/envd accepts out of the box; the
//! Connect envelope (`5-byte frame header`) is identical for both codecs. `PROTO` remains a
//! selectable variant for parity with the Java enum, but encoding the envd protobuf messages
//! would require the protobuf schema / `google.protobuf.DynamicMessage` equivalent and is
//! therefore rejected with [`SandboxExtensionError::Unsupported`] instead of silently
//! mis-encoding. See the module tracker for this recorded design decision.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use reqwest::{Client, Response};
use serde_json::{json, Value};

use crate::error::{SandboxErrorCode, SandboxExtensionError, SandboxResult};
use crate::http::join_url;
use crate::kubernetes_files::SandboxFileTransfer;
use crate::remote::{
    ensure_output_success, RemoteBackend, RemoteExecOutput, RemoteSandboxClient,
    RemoteSandboxHandle, TAR_TIMEOUT_SECONDS,
};
use crate::retry::{with_retries, RetryPolicy};
use crate::workspace::{shell_quote, short_hash};

/// Port envd listens on inside an E2B sandbox.
pub const ENVD_PORT: u16 = 49983;
/// Connect-Protocol framing: flag byte + 4-byte big-endian length.
pub const CONNECT_HEADER_LEN: usize = 5;
/// Connect envelope flag bit 0: the frame payload is compressed (unsupported here).
pub const CONNECT_FLAG_COMPRESSED: u8 = 0x01;
/// Connect envelope flag bit 1: end-of-stream trailer carrying an `EndStreamMessage`.
pub const CONNECT_FLAG_END_STREAM: u8 = 0x02;
/// Connect envelope flag bits 2-7: reserved for future extensions (skipped).
pub const CONNECT_FLAG_RESERVED: u8 = 0xfc;
/// Upper bound for a single Connect frame payload (mirrors the Java 64 MiB guard).
pub const MAX_CONNECT_FRAME_LEN: usize = 64 * 1024 * 1024;

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
    /// Protobuf codec (not implemented in Rust; selecting it is rejected — see module docs).
    Proto,
    /// JSON codec (default).
    ///
    /// This is the default **because it is the only codec this crate can encode**. Java defaults
    /// to `PROTO`; the Rust port keeps the variant for parity but makes the usable codec the
    /// default so a default-configured backend can execute commands.
    #[default]
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

    /// Parses the discriminator, defaulting to [`E2bCodec::Json`].
    pub fn parse(value: &str) -> Self {
        match value {
            "PROTO" => E2bCodec::Proto,
            _ => E2bCodec::Json,
        }
    }
}

/// A pre-existing E2B volume mounted into a sandbox at creation time.
///
/// Mirrors the Java `E2bVolumeMount` (commit `9730e8171`): `name` is the name of an
/// **existing** E2B volume and `path` is the absolute in-sandbox mount path. The platform
/// serializes the mount as `{"name": ..., "path": ...}` inside the `volumeMounts` array of
/// the `POST /sandboxes` body.
///
/// The volume lifecycle (create / delete / list, content read/write) stays in the E2B
/// dashboard or SDK; this type only describes *attach at create*.
///
/// The Java sibling `E2bCloudBucketMountStrategy` is an **empty placeholder interface**
/// (deprecated in the same commit), so no cloud-bucket mount source is implemented here
/// either — only the E2B volume source exists on both sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct E2bVolumeMount {
    /// Name of an existing E2B volume.
    pub name: String,
    /// Absolute in-sandbox mount path.
    pub path: String,
}

impl E2bVolumeMount {
    /// Creates a mount, validating the name and normalizing the path.
    ///
    /// The name must not be blank and the path must be absolute; both errors are
    /// configuration errors raised before any network call (mirrors the Java setters).
    pub fn new(name: impl Into<String>, path: impl Into<String>) -> SandboxResult<Self> {
        let name = name.into();
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(SandboxExtensionError::Configuration(
                "E2B volume mount name must be set (name of an existing volume)".to_string(),
            ));
        }
        let path = Self::normalize_mount_path(&path.into())?;
        Ok(Self { name, path })
    }

    /// Normalizes an in-sandbox mount path.
    ///
    /// Mirrors the Java `E2bVolumeMount#normalizeMountPath` (which delegates to
    /// `WorkspaceMountSupport#normalizeVirtualPath`): trim whitespace, turn backslashes
    /// into slashes, collapse repeated slashes and drop the trailing slash (except for the
    /// root itself). The path must be absolute.
    pub fn normalize_mount_path(path: &str) -> SandboxResult<String> {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return Err(SandboxExtensionError::Configuration(
                "E2B volume mount path must be set (absolute in-sandbox path, e.g. /mnt/data)"
                    .to_string(),
            ));
        }
        let mut normalized = String::with_capacity(trimmed.len());
        let mut previous_was_slash = false;
        for ch in trimmed.chars() {
            let ch = if ch == '\\' { '/' } else { ch };
            if ch == '/' {
                if previous_was_slash {
                    continue;
                }
                previous_was_slash = true;
            } else {
                previous_was_slash = false;
            }
            normalized.push(ch);
        }
        while normalized.len() > 1 && normalized.ends_with('/') {
            normalized.pop();
        }
        if !normalized.starts_with('/') {
            return Err(SandboxExtensionError::Configuration(format!(
                "E2B volume mount path must be absolute but was: {path}"
            )));
        }
        Ok(normalized)
    }

    /// Returns whether `workspace_root` lives on the given mount.
    ///
    /// Mirrors the Java `E2bVolumeMount#coversWorkspaceRoot`: the root equals the mount
    /// path or sits strictly beneath it. The trailing-slash guard keeps `/mnt/data-x` from
    /// matching a `/mnt/data` mount.
    pub fn covers_workspace_root(workspace_root: &str, mount_path: &str) -> bool {
        if workspace_root.trim().is_empty() || mount_path.trim().is_empty() {
            return false;
        }
        let Ok(root) = Self::normalize_mount_path(workspace_root) else {
            return false;
        };
        let Ok(mount) = Self::normalize_mount_path(mount_path) else {
            return false;
        };
        if mount == "/" {
            return true;
        }
        root == mount || root.starts_with(&format!("{mount}/"))
    }
}

/// Validates that no two mounts share the same in-sandbox path.
///
/// Mirrors the Java `E2bVolumeMount#requireDistinctMountPaths`.
pub fn require_distinct_mount_paths(mounts: &[E2bVolumeMount]) -> SandboxResult<()> {
    for (index, first) in mounts.iter().enumerate() {
        for second in mounts.iter().skip(index + 1) {
            if first.path == second.path {
                return Err(SandboxExtensionError::Configuration(format!(
                    "E2B volume mounts must use distinct paths, duplicated: {}",
                    first.path
                )));
            }
        }
    }
    Ok(())
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
    /// Fixed envd data-plane gateway URL, equivalent to the official SDK's `E2B_SANDBOX_URL`.
    ///
    /// When set, envd traffic for every sandbox is sent to this gateway and routed by the
    /// `E2b-Sandbox-Id` / `E2b-Sandbox-Port` request headers instead of a per-sandbox
    /// wildcard-DNS host derived from [`Self::domain`]. Control-plane calls still use
    /// [`Self::api_base_url`]. [`E2B_SANDBOX_URL_ENV`] is consulted when this is `None`.
    pub envd_sandbox_url: Option<String>,
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
    /// How many NATIVE_SNAPSHOT snapshots to keep per sandbox, pruning the oldest ones.
    ///
    /// `None` (the default) and `Some(0)` both disable pruning, matching the Java
    /// `snapshotRetention` default of `0` (`<= 0` disables). The window is applied to the ids this
    /// process created — E2B exposes create/delete but **no snapshot listing endpoint**, so
    /// snapshots written by earlier processes cannot be discovered, counted or pruned. See
    /// [`E2bBackend::prune_known_snapshots`].
    pub snapshot_retention: Option<usize>,
    /// Pre-existing E2B volumes mounted at sandbox creation time.
    ///
    /// Mirrors the Java `E2bSandboxClientOptions#volumeMounts`. Every entry is serialized as
    /// `{"name": ..., "path": ...}` inside the `volumeMounts` array of the `POST /sandboxes`
    /// body; the field is omitted entirely when the list is empty.
    ///
    /// The Java cloud-bucket mount strategy (`E2bCloudBucketMountStrategy`) is an empty
    /// placeholder interface, so no cloud-bucket source is modelled here either.
    pub volume_mounts: Vec<E2bVolumeMount>,
    /// envd Connect codec.
    ///
    /// Defaults to [`E2bCodec::Json`] (the only codec this crate can encode), unlike the Java
    /// `E2bCodec.PROTO` default — see [`E2bCodec`].
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
            envd_sandbox_url: None,
            template_id: "base".to_string(),
            workspace_root: "/home/user".to_string(),
            sandbox_timeout_seconds: 300,
            run_user: "user".to_string(),
            persistence_mode: E2bPersistenceMode::Tar,
            snapshot_retention: None,
            volume_mounts: Vec::new(),
            codec: E2bCodec::Json,
            connect_timeout_seconds: 30,
            read_timeout_seconds: 120,
            max_retries: 3,
        }
    }
}

impl E2bSandboxClientOptions {
    /// Adds a volume mount, validating it and keeping the mount paths distinct.
    ///
    /// Mirrors the Java `E2bSandboxClientOptions#addVolumeMount` (which also runs the
    /// distinct-path check of `setVolumeMounts`).
    pub fn with_volume_mount(
        mut self,
        name: impl Into<String>,
        path: impl Into<String>,
    ) -> SandboxResult<Self> {
        let mount = E2bVolumeMount::new(name, path)?;
        self.volume_mounts.push(mount);
        require_distinct_mount_paths(&self.volume_mounts)?;
        Ok(self)
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

/// Builds the `POST /sandboxes` request body (without volume mounts).
///
/// Kept as the legacy two-argument shape: a sandbox created without mounts must produce
/// exactly `{templateID, timeout}`. See [`build_create_sandbox_body_with_mounts`].
pub fn build_create_sandbox_body(template_id: &str, timeout_seconds: u64) -> Value {
    build_create_sandbox_body_with_mounts(template_id, timeout_seconds, &[])
}

/// Builds the `POST /sandboxes` request body, mounting pre-existing volumes when given.
///
/// Mirrors the Java `E2bPlatformHttp#createSandbox` (commit `9730e8171`): the mounts are
/// serialized as `volumeMounts: [{name, path}]` and the field is **omitted** when no mount
/// is configured, so the legacy request body stays byte-identical.
pub fn build_create_sandbox_body_with_mounts(
    template_id: &str,
    timeout_seconds: u64,
    mounts: &[E2bVolumeMount],
) -> Value {
    let mut body = json!({ "templateID": template_id, "timeout": timeout_seconds });
    if !mounts.is_empty() {
        let entries: Vec<Value> = mounts
            .iter()
            .map(|mount| json!({ "name": mount.name, "path": mount.path }))
            .collect();
        body["volumeMounts"] = Value::Array(entries);
    }
    body
}

/// Builds the `POST /sandboxes/{id}/connect` request body.
pub fn build_connect_sandbox_body(timeout_seconds: u64) -> Value {
    json!({ "timeout": timeout_seconds })
}

/// Slack between the server-side kill deadline and the client-side backup deadline.
///
/// Mirrors the Java `E2bEnvdProcessClient.CLIENT_TIMEOUT_SLACK_MILLIS` (5 s): the server
/// deadline (`Connect-Timeout-Ms`) fires first and kills the remote process, the client
/// backup only guarantees a bounded return when the server never answers (mishaved server
/// or hung stream) and absorbs connect/setup jitter.
pub const CLIENT_TIMEOUT_SLACK_SECONDS: u64 = 5;

/// Connect error code envd returns when the server-side exec deadline fired.
pub const CONNECT_DEADLINE_EXCEEDED: &str = "deadline_exceeded";

/// Computes the Connect `Connect-Timeout-Ms` value that arms the server-side kill.
///
/// E2B's data plane exposes no "kill a running process" endpoint for a process started
/// through `process.Process/Start`: the kill is requested **when the process starts**, by
/// attaching the Connect deadline to the start request, and envd terminates the remote
/// process once that deadline lapses (Java `E2bEnvdProcessClient`, commit `aa66abfd0`).
/// Without the header a client-side timeout only drops the HTTP stream and leaks the
/// process.
///
/// `timeout_seconds == 0` means "no deadline" and yields `None`: the header must be omitted
/// because `Connect-Timeout-Ms: 0` means "deadline already exceeded", not "no deadline".
pub fn connect_timeout_ms_header(timeout_seconds: u64) -> Option<String> {
    if timeout_seconds == 0 {
        return None;
    }
    Some(timeout_seconds.saturating_mul(1000).to_string())
}

/// The overall client-side deadline of an exec call.
///
/// `None` means "no exec deadline": the transport-level `read_timeout_seconds` stays the
/// only bound (the legacy behaviour). Otherwise the deadline is the requested timeout plus
/// [`CLIENT_TIMEOUT_SLACK_SECONDS`], mirroring the Java per-call `callTimeout`: the server
/// deadline fires first (and kills the process), the slack only guarantees a bounded
/// return when the server never answers.
pub fn exec_client_deadline(timeout_seconds: u64) -> Option<Duration> {
    if timeout_seconds == 0 {
        return None;
    }
    Some(Duration::from_secs(
        timeout_seconds.saturating_add(CLIENT_TIMEOUT_SLACK_SECONDS),
    ))
}

/// Runs `fut` under the exec deadline, mapping an expired deadline onto an exec timeout.
///
/// `timeout_seconds == 0` means "no deadline" and leaves `fut` untouched (the
/// transport-level timeout remains the only bound). The error message never contains the
/// command text: commands are model-generated and may inline secrets (same choice as the
/// Java `SandboxException.ExecTimeoutException`).
pub async fn with_exec_deadline<T, F>(timeout_seconds: u64, fut: F) -> SandboxResult<T>
where
    F: std::future::Future<Output = SandboxResult<T>>,
{
    let Some(deadline) = exec_client_deadline(timeout_seconds) else {
        return fut.await;
    };
    match tokio::time::timeout(deadline, fut).await {
        Ok(result) => result,
        Err(_elapsed) => Err(exec_timeout_error(timeout_seconds)),
    }
}

/// Builds the exec timeout error for `timeout_seconds`.
pub fn exec_timeout_error(timeout_seconds: u64) -> SandboxExtensionError {
    SandboxExtensionError::Runtime(
        SandboxErrorCode::ExecTimeout,
        format!("Command timed out after {timeout_seconds}s"),
    )
}

/// Maps a transport failure of an exec call onto the exec error taxonomy.
///
/// A reqwest timeout means the transport gave up on the whole request. When an exec
/// deadline was requested it is reported as an exec timeout (the remote process was killed
/// server-side through the `Connect-Timeout-Ms` deadline); without a deadline it stays a
/// plain transport error, because no deadline was ever asked for.
fn map_exec_transport_error(err: reqwest::Error, timeout_seconds: u64) -> SandboxExtensionError {
    if err.is_timeout() && timeout_seconds > 0 {
        return exec_timeout_error(timeout_seconds);
    }
    SandboxExtensionError::Transport(err.to_string())
}

/// Server-side kill channel for an envd process.
///
/// The E2B data plane has no "kill a running process" endpoint: the kill is requested when
/// the process starts (see [`connect_timeout_ms_header`]) and envd terminates the remote
/// process once the deadline lapses. The trait isolates that request so the exec path can
/// be exercised offline with a fake recording the requested deadline.
pub trait E2bKillDeadline: Send + Sync + std::fmt::Debug {
    /// Returns the `Connect-Timeout-Ms` value (in milliseconds) that arms the server-side
    /// kill, or `None` when no kill must be armed.
    fn deadline_ms(&self, timeout_seconds: u64) -> Option<u64>;
}

/// Production kill channel: the Connect deadline header value.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvdKillDeadline;

impl E2bKillDeadline for EnvdKillDeadline {
    fn deadline_ms(&self, timeout_seconds: u64) -> Option<u64> {
        if timeout_seconds == 0 {
            return None;
        }
        Some(timeout_seconds.saturating_mul(1000))
    }
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
    let dom = if domain.trim().is_empty() {
        "e2b.app"
    } else {
        domain
    };
    format!("https://{ENVD_PORT}-{sandbox_id}.{dom}")
}

/// Environment variable overriding the envd data-plane URL (the official SDK's `E2B_SANDBOX_URL`).
pub const E2B_SANDBOX_URL_ENV: &str = "E2B_SANDBOX_URL";

fn trim_url(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.trim_end_matches('/').to_string())
    }
}

/// Resolves the envd data-plane base URL: explicit option > environment override > per-sandbox host.
///
/// Mirrors `E2bEnvdProcessClient#envdHost` after the Java `E2B_SANDBOX_URL` commit: a configured
/// gateway wins over the wildcard-DNS address built by [`envd_host`], and blank values are ignored.
pub fn resolve_envd_base_url(
    override_url: Option<&str>,
    env_url: Option<&str>,
    sandbox_id: &str,
    domain: &str,
) -> String {
    if let Some(url) = override_url.and_then(trim_url) {
        return url;
    }
    if let Some(url) = env_url.and_then(trim_url) {
        return url;
    }
    envd_host(sandbox_id, domain)
}

/// Reads the [`E2B_SANDBOX_URL_ENV`] override through an injectable lookup (offline-testable).
pub fn envd_sandbox_url_from<F>(lookup: F) -> Option<String>
where
    F: FnOnce(&str) -> Option<String>,
{
    lookup(E2B_SANDBOX_URL_ENV).and_then(|value| trim_url(&value))
}

/// Reads the [`E2B_SANDBOX_URL_ENV`] process environment variable.
pub fn envd_sandbox_url_from_env() -> Option<String> {
    envd_sandbox_url_from(|key| std::env::var(key).ok())
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

/// One decoded Connect envelope, with the raw flag byte preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectEnvelope {
    /// Connect flag byte (bit 0 = compressed, bit 1 = end stream, bits 2-7 reserved).
    pub flags: u8,
    /// Frame payload bytes.
    pub payload: Vec<u8>,
}

/// Splits a Connect response body into envelopes, tolerating fragmented/truncated streams.
///
/// Hardened against the failure modes the Java `E2bEnvdProcessClient` fixes address
/// (commit `897c4866e`):
///
/// * the length prefix is read as an *unsigned* 4-byte big-endian integer (no erroneous
///   `& 0x7fff_ffff` mask),
/// * a payload that runs past the end of the buffer (a frame still in flight, or a truncated
///   body) stops decoding **without panicking**, keeping every complete envelope decoded so far,
/// * an implausible length (> [`MAX_CONNECT_FRAME_LEN`]) stops decoding instead of allocating,
/// * reserved flag bits (`0xfc`) are skipped rather than surfaced as data frames.
///
/// Compressed (`0x01`) and end-stream (`0x02`) envelopes are returned to the caller so it can
/// decide whether they are fatal; [`decode_connect_frames`] filters them out.
pub fn decode_connect_envelopes(data: &[u8]) -> Vec<ConnectEnvelope> {
    let mut envelopes = Vec::new();
    let mut cursor = 0usize;
    while cursor + CONNECT_HEADER_LEN <= data.len() {
        let flags = data[cursor];
        let len = u32::from_be_bytes([
            data[cursor + 1],
            data[cursor + 2],
            data[cursor + 3],
            data[cursor + 4],
        ]) as usize;
        cursor += CONNECT_HEADER_LEN;
        if len > MAX_CONNECT_FRAME_LEN {
            break;
        }
        let Some(end_of_payload) = cursor.checked_add(len) else {
            break;
        };
        if end_of_payload > data.len() {
            // Partial frame: keep what we have and stop (no panic, no partial payload).
            break;
        }
        let payload = data[cursor..end_of_payload].to_vec();
        cursor = end_of_payload;
        if flags & CONNECT_FLAG_RESERVED != 0 {
            continue;
        }
        envelopes.push(ConnectEnvelope { flags, payload });
    }
    envelopes
}

/// Splits a Connect response body into data-frame payloads (skipping trailers/invalid frames).
pub fn decode_connect_frames(data: &[u8]) -> Vec<Vec<u8>> {
    decode_connect_envelopes(data)
        .into_iter()
        .filter(|envelope| envelope.flags == 0x00)
        .map(|envelope| envelope.payload)
        .collect()
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
        // protojson accepts both the camelCase `jsonName` and the original proto field name.
        out.exit_code = end
            .get("exitCode")
            .or_else(|| end.get("exit_code"))
            .and_then(Value::as_i64)
            .map(|v| v as i32);
        // `EndEvent.error` is populated when the process was signalled or exited non-zero
        // (`signal: killed`, `exit status N`); surface it on stderr, visually separated from
        // stderr output that may not end with a newline.
        if let Some(error) = end
            .get("error")
            .and_then(Value::as_str)
            .filter(|e| !e.is_empty())
        {
            append_separated_line(&mut out.stderr, error.as_bytes());
        }
    }
    Ok(out)
}

/// Appends `text` to `out`, inserting a newline when `out` does not already end with one.
fn append_separated_line(out: &mut Vec<u8>, text: &[u8]) {
    if !out.is_empty() && out.last() != Some(&b'\n') {
        out.push(b'\n');
    }
    out.extend_from_slice(text);
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
///
/// Corrupt *data* frames are skipped (a single bad frame must not abort a stream that still
/// yields an exit code), while stream-level conditions are fatal — matching the Java
/// `drainStartStream` contract:
///
/// * a compressed frame (`0x01`) is rejected as [`SandboxExtensionError::Unsupported`],
/// * an end-stream trailer (`0x02`) whose `EndStreamMessage` carries a truthy `error` is rejected
///   as [`SandboxErrorCode::WorkspaceStartError`] (a `deadline_exceeded` code is reported as
///   [`SandboxErrorCode::ExecTimeout`]: the server killed the process on its deadline),
/// * an unterminated stream yields `exit_code == None` so the caller can report it.
pub fn drain_envd_stream(data: &[u8]) -> SandboxResult<EnvdStreamResult> {
    let mut result = EnvdStreamResult::default();
    for envelope in decode_connect_envelopes(data) {
        if envelope.flags & CONNECT_FLAG_COMPRESSED != 0 {
            return Err(SandboxExtensionError::Unsupported(
                "compressed envd connect frames are not supported".to_string(),
            ));
        }
        if envelope.flags & CONNECT_FLAG_END_STREAM != 0 {
            // A server-side exec deadline kill surfaces here as `deadline_exceeded`; it is
            // an exec timeout, not a generic stream failure (Java `ConnectStreamException`).
            if parse_end_stream_error_code(&envelope.payload).as_deref()
                == Some(CONNECT_DEADLINE_EXCEEDED)
            {
                return Err(SandboxExtensionError::Runtime(
                    SandboxErrorCode::ExecTimeout,
                    format!("envd process stream exceeded the server-side exec deadline ({CONNECT_DEADLINE_EXCEEDED})"),
                ));
            }
            // A trailer that is not valid `EndStreamMessage` JSON is tolerated here (Java throws):
            // the stream itself is complete, so a missing exit code is reported by the caller.
            if let Some(description) = parse_end_stream_error(&envelope.payload) {
                return Err(SandboxExtensionError::Runtime(
                    SandboxErrorCode::WorkspaceStartError,
                    format!("process start failed: {description}"),
                ));
            }
            continue;
        }
        if let Ok(event) = parse_envd_event(&envelope.payload) {
            result.stdout.extend_from_slice(&event.stdout);
            result.stderr.extend_from_slice(&event.stderr);
            if let Some(code) = event.exit_code {
                result.exit_code = Some(code);
            }
        }
    }
    Ok(result)
}

/// Parses a Connect `EndStreamMessage` trailer, returning its error description when present.
///
/// Mirrors the official Connect reader (and the Python SDK's `ServerStreamParser`): the `error`
/// key's presence alone is not enough — the value must be *truthy* (missing / `null` / empty
/// object / empty string / `0` / `false` all mean success).
pub fn parse_end_stream_error(payload: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(payload).ok()?;
    let error = value.get("error")?;
    if is_falsy(error) {
        return None;
    }
    let code = error.get("code").and_then(Value::as_str);
    let message = error.get("message").and_then(Value::as_str);
    Some(match (code, message) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code.to_string(),
        (None, Some(message)) => message.to_string(),
        (None, None) => error.to_string(),
    })
}

/// Parses the raw Connect error code of an `EndStreamMessage` trailer.
///
/// Returns `None` when the trailer carries no error, no code, or a falsy error value (the
/// Connect truthiness rules). Unlike [`parse_end_stream_error`] the code is returned
/// verbatim, so the caller can tell the server-side deadline kill (`deadline_exceeded`) from
/// other stream failures.
pub fn parse_end_stream_error_code(payload: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(payload).ok()?;
    let error = value.get("error")?;
    if is_falsy(error) {
        return None;
    }
    error
        .get("code")
        .and_then(Value::as_str)
        .map(|code| code.to_string())
}

/// Connect / Python-SDK truthiness: missing, `null`, empty containers, `""`, `0` and `false`
/// all mean "no error".
fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(flag) => !*flag,
        Value::Number(number) => number.as_f64().map(|n| n == 0.0).unwrap_or(false),
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
    }
}

/// E2B [`RemoteBackend`] implementation.
///
/// Cheap to clone: every field is an `Arc` / reference-counted handle, so the native
/// file-transfer adapter can own a snapshot of the backend next to the live sandbox handle.
#[derive(Debug, Clone)]
pub struct E2bBackend {
    options: E2bSandboxClientOptions,
    client: Client,
    retry: RetryPolicy,
    /// Channel asking envd to kill the remote process when the exec deadline lapses.
    ///
    /// Injectable so the exec timeout path can be exercised offline with a fake recording
    /// the requested deadline (see [`E2bKillDeadline`]).
    kill_deadline: Arc<dyn E2bKillDeadline>,
    /// Snapshot ids created through this backend instance, oldest first.
    ///
    /// E2B exposes create (`POST /sandboxes/{id}/snapshots`) and delete
    /// (`DELETE /templates/{id}`) but **no listing endpoint**, so this record is the only view of
    /// "which snapshots exist" available to the retention window. It is process-local: after a
    /// restart the list starts empty again and pre-existing snapshots become invisible.
    known_snapshots: Arc<Mutex<Vec<String>>>,
}

impl E2bBackend {
    /// Builds a backend, wiring up the reqwest client and retry policy.
    pub fn new(options: E2bSandboxClientOptions) -> SandboxResult<Self> {
        Self::with_kill_deadline(options, Arc::new(EnvdKillDeadline))
    }

    /// Builds a backend with an injected server-side kill channel.
    ///
    /// The production channel is [`EnvdKillDeadline`]; tests inject a recording fake to
    /// assert that the exec timeout really asks the platform to kill the process.
    pub fn with_kill_deadline(
        options: E2bSandboxClientOptions,
        kill_deadline: Arc<dyn E2bKillDeadline>,
    ) -> SandboxResult<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(options.connect_timeout_seconds))
            .timeout(Duration::from_secs(options.read_timeout_seconds))
            .build()
            .map_err(|e| {
                SandboxExtensionError::Transport(format!("failed to build client: {e}"))
            })?;
        let retry = RetryPolicy::e2b(options.max_retries);
        Ok(Self {
            options,
            client,
            retry,
            kill_deadline,
            known_snapshots: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Locks the snapshot record, tolerating poisoning (the record is plain data, never
    /// invariants-bearing, so a panicked thread must not take the backend down with it).
    fn known_snapshots_guard(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.known_snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Returns the snapshot ids this process created, oldest first.
    pub fn known_snapshot_ids(&self) -> Vec<String> {
        self.known_snapshots_guard().clone()
    }

    /// Records a snapshot id this process created (most recent last).
    ///
    /// Blank ids are ignored and duplicates keep their original (oldest) position, mirroring the
    /// Java `LinkedHashSet` de-duplication where the first occurrence wins.
    pub fn note_snapshot(&self, snapshot_id: &str) {
        let id = snapshot_id.trim();
        if id.is_empty() {
            return;
        }
        let mut ids = self.known_snapshots_guard();
        if ids.iter().any(|existing| existing == id) {
            return;
        }
        ids.push(id.to_string());
    }

    /// Deletes one snapshot template via `DELETE /templates/{snapshotId}`.
    ///
    /// The id is percent-encoded as a **single path segment**, so a namespaced id such as
    /// `team/alias:tag` becomes `/templates/team%2Falias:tag`; an unencoded `/` would be parsed as
    /// an extra segment and E2B answers 404 (mirrors the Java `HttpUrl.addPathSegment` fix).
    ///
    /// Unlike the Java helper, a 404 is surfaced as an error instead of being swallowed, so the
    /// caller can tell "deleted" from "never existed". Callers performing best-effort pruning
    /// treat `status() == Some(404)` as already-absent; see [`Self::prune_snapshot_ids`].
    pub async fn delete_snapshot(&self, snapshot_id: &str) -> SandboxResult<()> {
        let template = snapshot_id.trim();
        if template.is_empty() {
            return Err(SandboxExtensionError::Configuration(
                "E2B snapshot id must not be blank".to_string(),
            ));
        }
        let url = join_url(
            &self.options.api_base_url,
            &format!("/templates/{}", encode_path_segment(template)),
        );
        let api_key = self.api_key()?;
        let template = template.to_string();
        let client = self.client.clone();
        let retry = self.retry.clone();
        with_retries(&retry, move || {
            let client = client.clone();
            let url = url.clone();
            let api_key = api_key.clone();
            let template = template.clone();
            async move {
                let response = client
                    .delete(&url)
                    .header("X-API-Key", api_key)
                    .send()
                    .await
                    .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
                let status = response.status().as_u16();
                if response.status().is_success() {
                    return Ok(());
                }
                let body = response.text().await.unwrap_or_default();
                let detail = body.trim();
                let suffix = if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                };
                Err(match status {
                    401 | 403 => SandboxExtensionError::http(
                        status,
                        format!(
                            "E2B snapshot delete unauthorized for template {template} (HTTP {status}); \
                             check E2bSandboxClientOptions::api_key{suffix}"
                        ),
                    ),
                    404 => SandboxExtensionError::http(
                        404,
                        format!(
                            "E2B snapshot delete failed: template {template} not found (HTTP 404); \
                             it may already be deleted, or the id/namespace is wrong{suffix}"
                        ),
                    ),
                    _ => SandboxExtensionError::http(
                        status,
                        format!("E2B snapshot delete failed for template {template} (HTTP {status}){suffix}"),
                    ),
                })
            }
        })
        .await
    }

    /// Applies the configured [`E2bSandboxClientOptions::snapshot_retention`] window to the ids
    /// this process recorded, deleting the oldest overflow and updating the record.
    ///
    /// No-op (and no HTTP traffic) when retention is unset or `0`. Pruning is best-effort: an
    /// individual failure is logged and the id is kept for a later attempt, so it never fails the
    /// persisting caller. A 404 counts as "already gone" and is dropped.
    ///
    /// # Capability boundary
    ///
    /// E2B has no snapshot listing endpoint, so this can **only** prune snapshots created by this
    /// process. Snapshots left behind by a previous process (or by another host) are invisible
    /// here and must be deleted explicitly with [`Self::delete_snapshot`].
    pub async fn prune_known_snapshots(&self) -> SandboxResult<()> {
        let retention = match self.options.snapshot_retention {
            Some(n) if n > 0 => n,
            _ => return Ok(()),
        };
        let ids = self.known_snapshot_ids();
        let kept = self.prune_snapshot_ids(&ids, retention).await;
        *self.known_snapshots_guard() = kept;
        Ok(())
    }

    /// Keeps the last `retention` ids (most recent last) and deletes the rest, returning the ids
    /// that remain known (surviving prune window plus any id whose delete failed).
    ///
    /// `retention == 0` disables pruning and returns the normalized input untouched.
    pub async fn prune_snapshot_ids(
        &self,
        snapshot_ids: &[String],
        retention: usize,
    ) -> Vec<String> {
        let ids = dedupe_snapshot_ids(snapshot_ids);
        if retention == 0 || ids.len() <= retention {
            return ids;
        }
        let split = ids.len() - retention;
        let mut kept: Vec<String> = Vec::new();
        for id in &ids[..split] {
            let outcome = self.delete_snapshot(id).await;
            match outcome {
                Ok(()) => {
                    tracing::debug!(snapshot = %id, "e2b: pruned snapshot");
                }
                Err(err) if err.status() == Some(404) => {
                    tracing::debug!(snapshot = %id, "e2b: snapshot already absent ({err})");
                }
                Err(err) => {
                    tracing::warn!(snapshot = %id, "e2b: failed to prune snapshot: {err}");
                    kept.push(id.clone());
                }
            }
        }
        kept.extend(ids[split..].iter().cloned());
        kept
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

    /// Resolves the envd data-plane base URL for a handle.
    ///
    /// Precedence: the handle's own `envd_sandbox_url` (restored state) wins, then the client
    /// option, then the [`E2B_SANDBOX_URL_ENV`] process environment variable, then the
    /// per-sandbox wildcard-DNS host. Mirrors the Java `E2bEnvdProcessClient#envdHost`.
    fn envd_base_url(&self, handle: &RemoteSandboxHandle) -> String {
        let handle_url = handle.extra.get("envd_sandbox_url").map(String::as_str);
        let env_url = envd_sandbox_url_from_env();
        resolve_envd_base_url(
            handle_url.or(self.options.envd_sandbox_url.as_deref()),
            env_url.as_deref(),
            &handle.id,
            &self.domain_from(handle),
        )
    }

    /// Builds the envd `/files` REST URL for `remote_path`, mirroring the Java `filesUrl`.
    ///
    /// `path` and `username` are percent-encoded with the `application/x-www-form-urlencoded`
    /// alphabet used by the Java `URLEncoder`, so `/` becomes `%2F` exactly as Java produces.
    pub fn files_url(&self, remote_path: &str) -> String {
        format!(
            "/files?path={}&username={}",
            encode_query_component(remote_path),
            encode_query_component(&self.effective_run_user())
        )
    }

    fn effective_run_user(&self) -> String {
        let user = self.options.run_user.trim();
        if user.is_empty() {
            "user".to_string()
        } else {
            user.to_string()
        }
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

    async fn create_from_template(
        &self,
        template_id: &str,
    ) -> SandboxResult<BTreeMap<String, String>> {
        let body = build_create_sandbox_body_with_mounts(
            template_id,
            self.options.sandbox_timeout_seconds,
            &self.options.volume_mounts,
        );
        let value = self.platform_post("/sandboxes", &body).await?;
        Ok(parse_sandbox_fields(&value))
    }

    /// Uploads a file to the sandbox via the envd `/files` REST API (multipart form POST).
    ///
    /// Mirrors the Java `E2bEnvdProcessClient#uploadFile`: parent directories are created
    /// automatically and existing files are overwritten by envd itself. The request carries the
    /// same `E2b-Sandbox-Id` / `E2b-Sandbox-Port` / `X-Access-Token` / `Authorization` headers as
    /// the Connect exec path, so a fixed gateway can route it.
    pub async fn upload_file(
        &self,
        handle: &RemoteSandboxHandle,
        remote_path: &str,
        data: &[u8],
    ) -> SandboxResult<()> {
        let filename = filename_from_path(remote_path);
        if filename.is_empty() {
            return Err(SandboxExtensionError::Configuration(format!(
                "remotePath must be a file path: {remote_path}"
            )));
        }
        let url = join_url(&self.envd_base_url(handle), &self.files_url(remote_path));
        let payload = Arc::new(data.to_vec());
        let filename = filename.to_string();
        let build_url = url.clone();
        let build = move || {
            let part = reqwest::multipart::Part::bytes((*payload).clone())
                .file_name(filename.clone())
                .mime_str("application/octet-stream")
                .expect("application/octet-stream is always a valid mime type");
            let form = reqwest::multipart::Form::new().part("file", part);
            self.envd_request(reqwest::Method::POST, &build_url, handle)
                .multipart(form)
        };
        self.execute_envd(&url, SandboxErrorCode::WorkspaceArchiveWriteError, build)
            .await
            .map(|_| ())
    }

    /// Downloads a file from the sandbox via the envd `/files` REST API (GET).
    ///
    /// Mirrors the Java `E2bEnvdProcessClient#downloadFile`.
    pub async fn download_file(
        &self,
        handle: &RemoteSandboxHandle,
        remote_path: &str,
    ) -> SandboxResult<Vec<u8>> {
        let url = join_url(&self.envd_base_url(handle), &self.files_url(remote_path));
        let build_url = url.clone();
        let build = move || self.envd_request(reqwest::Method::GET, &build_url, handle);
        self.execute_envd(&url, SandboxErrorCode::WorkspaceArchiveReadError, build)
            .await
    }

    /// Archives the workspace through envd: `tar` into a temp file, fetch it with the file API.
    ///
    /// Mirrors the Java `E2bSandbox#doPersistWorkspace` TAR branch (envd `runShell` + `downloadFile`)
    /// rather than piping base64 through stdout, so large workspaces are not limited by command
    /// output buffers. The temp archive is removed afterwards, best effort.
    async fn persist_tar_via_files(
        &self,
        handle: &RemoteSandboxHandle,
        root: &str,
    ) -> SandboxResult<Vec<u8>> {
        let tar_path = new_temp_tar_path();
        let command = format!(
            "tar -cf {} -C {} .",
            shell_quote(&tar_path),
            shell_quote(root)
        );
        let archive = match self
            .exec_in(handle, &command, None, TAR_TIMEOUT_SECONDS)
            .await
        {
            Ok(output) => match ensure_output_success("workspace archive", &output) {
                Ok(()) => self.download_file(handle, &tar_path).await,
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        };
        let _ = self
            .exec_in(
                handle,
                &format!("rm -f {}", shell_quote(&tar_path)),
                None,
                30,
            )
            .await;
        archive
    }

    /// Restores an archive through envd: upload it to a temp file, then `tar -xf` it.
    ///
    /// Mirrors the Java `E2bSandbox#doHydrateWorkspace` TAR branch (envd `uploadFile` + `runShell`).
    async fn hydrate_tar_via_files(
        &self,
        handle: &RemoteSandboxHandle,
        root: &str,
        archive: &[u8],
    ) -> SandboxResult<()> {
        let tar_path = new_temp_tar_path();
        let extracted = match self.upload_file(handle, &tar_path, archive).await {
            Ok(()) => {
                let command = format!(
                    "tar -xf {} -C {}",
                    shell_quote(&tar_path),
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
                &format!("rm -f {}", shell_quote(&tar_path)),
                None,
                30,
            )
            .await;
        extracted
    }

    /// Builds an envd request with the shared data-plane headers (`baseRequest` in Java).
    fn envd_request(
        &self,
        method: reqwest::Method,
        url: &str,
        handle: &RemoteSandboxHandle,
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .request(method, url)
            .header("User-Agent", "agentscope-rust-e2b")
            .header("E2b-Sandbox-Id", handle.id.clone())
            .header("E2b-Sandbox-Port", ENVD_PORT.to_string())
            .header("Authorization", basic_auth_header(&self.options.run_user));
        if let Some(token) = handle
            .extra
            .get("envd_access_token")
            .filter(|t| !t.trim().is_empty())
        {
            request = request.header("X-Access-Token", token.clone());
        }
        request
    }

    /// Sends an envd request, mapping non-success statuses onto `code` and returning the body.
    ///
    /// `build` is re-invoked for every attempt because multipart bodies are streaming and cannot
    /// be replayed; the resulting request is therefore built fresh each time.
    async fn execute_envd<F>(
        &self,
        url: &str,
        code: SandboxErrorCode,
        build: F,
    ) -> SandboxResult<Vec<u8>>
    where
        F: Fn() -> reqwest::RequestBuilder + Clone,
    {
        let client = self.client.clone();
        let retry = self.retry.clone();
        let url = url.to_string();
        with_retries(&retry, move || {
            let client = client.clone();
            let url = url.clone();
            let build = build.clone();
            async move {
                let request = build().build().map_err(|e| {
                    SandboxExtensionError::Transport(format!(
                        "failed to build request for {url}: {e}"
                    ))
                })?;
                send_envd(&client, request, &url, code).await
            }
        })
        .await
    }
}

/// Executes one envd request and decodes the response body.
async fn send_envd(
    client: &Client,
    request: reqwest::Request,
    url: &str,
    code: SandboxErrorCode,
) -> SandboxResult<Vec<u8>> {
    let response = client
        .execute(request)
        .await
        .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
    if !status.is_success() {
        return Err(SandboxExtensionError::Runtime(
            code,
            format!(
                "envd filesystem request failed HTTP {} from {url}: {}",
                status.as_u16(),
                String::from_utf8_lossy(&bytes)
            ),
        ));
    }
    Ok(bytes.to_vec())
}

/// Builds a unique in-sandbox tar path (mirrors the Java `E2bSandbox#newTempTarPath`).
pub fn new_temp_tar_path() -> String {
    format!("/tmp/agentscope-ws-{}.tar", uuid::Uuid::new_v4())
}

/// Maps an absolute sandbox path to the envd file-transfer path, or `None` when untransferable.
///
/// Mirrors the Java `E2bSandbox#workspaceTransferPath`: the path must be absolute, live strictly
/// beneath the workspace root (which must itself be absolute) and contain no empty, `.` or `..`
/// segments — those escape the workspace and are left to the exec transfer strategy. The
/// returned value is the absolute path handed to the envd `/files` endpoint. Unlike the Java
/// code, a backslash is treated as a literal path character (Java
/// `WorkspaceMountSupport.normalizeVirtualPath` rewrites it to `/`); both sides therefore reject
/// the same traversal sequences.
pub fn workspace_transfer_path(workspace_root: &str, absolute_path: &str) -> Option<String> {
    let path = absolute_path.trim();
    if path.is_empty() || !path.starts_with('/') {
        return None;
    }
    let root = workspace_root.trim();
    if root.is_empty() || !root.starts_with('/') {
        return None;
    }
    let normalized_root = E2bVolumeMount::normalize_mount_path(root).ok()?;
    let root_prefix = if normalized_root == "/" {
        "/".to_string()
    } else {
        format!("{normalized_root}/")
    };
    let relative = path.strip_prefix(&root_prefix)?;
    if relative.is_empty() {
        return None;
    }
    if relative
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return None;
    }
    Some(format!("{root_prefix}{relative}"))
}

/// `SandboxFileTransfer` backed by the envd `/files` REST API.
///
/// Mirrors the Java `E2bSandbox implements SandboxFileTransfer`: only paths beneath the workspace
/// root are accepted (see [`workspace_transfer_path`]); everything else falls back to the
/// base64-over-exec strategy in the filesystem adapter.
#[derive(Debug, Clone)]
pub struct E2bFileTransfer {
    backend: E2bBackend,
    handle: RemoteSandboxHandle,
}

impl E2bFileTransfer {
    /// Creates an adapter for a live sandbox handle.
    pub fn new(backend: E2bBackend, handle: RemoteSandboxHandle) -> Self {
        Self { backend, handle }
    }

    /// Returns the wrapped live handle.
    pub fn handle(&self) -> &RemoteSandboxHandle {
        &self.handle
    }

    /// Resolves the transfer path, failing like the Java `requireWorkspaceTransferPath`.
    fn require_transfer_path(&self, absolute_path: &str) -> SandboxResult<String> {
        workspace_transfer_path(&self.backend.options.workspace_root, absolute_path).ok_or_else(
            || {
                SandboxExtensionError::Configuration(format!(
                    "path is outside the sandbox workspace or invalid: {absolute_path}"
                ))
            },
        )
    }
}

#[async_trait::async_trait]
impl SandboxFileTransfer for E2bFileTransfer {
    fn supports_file_transfer(&self, absolute_path: &str) -> bool {
        workspace_transfer_path(&self.backend.options.workspace_root, absolute_path).is_some()
    }

    async fn upload_file(&self, absolute_path: &str, content: &[u8]) -> SandboxResult<()> {
        let path = self.require_transfer_path(absolute_path)?;
        self.backend.upload_file(&self.handle, &path, content).await
    }

    async fn download_file(&self, absolute_path: &str) -> SandboxResult<Vec<u8>> {
        let path = self.require_transfer_path(absolute_path)?;
        self.backend.download_file(&self.handle, &path).await
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
        fields.insert(
            "persistence_mode".to_string(),
            self.options.persistence_mode.as_str().to_string(),
        );
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
        fields.insert(
            "persistence_mode".to_string(),
            self.options.persistence_mode.as_str().to_string(),
        );
        fields.insert("codec".to_string(), self.options.codec.as_str().to_string());
        Ok(RemoteSandboxHandle::new(sandbox_id.to_string(), fields))
    }

    async fn kill_sandbox(&self, sandbox_id: &str) -> SandboxResult<()> {
        let url = join_url(
            &self.options.api_base_url,
            &format!("/sandboxes/{sandbox_id}"),
        );
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
        let started = std::time::Instant::now();
        let host = self.envd_base_url(handle);
        let url = join_url(&host, "process.Process/Start");
        let payload = serde_json::to_vec(&build_envd_start_json_body(command, cwd))
            .map_err(|e| SandboxExtensionError::Serialization(e.to_string()))?;
        let envelope = encode_connect_envelope(&payload);
        let auth = basic_auth_header(&self.options.run_user);
        let access_token = handle.extra.get("envd_access_token").cloned();
        let sandbox_id = handle.id.clone();
        let client = self.client.clone();
        let retry = self.retry.clone();
        // 服务端 kill：把 Connect deadline 绑到 start 请求上，envd 到点终止远端进程。
        // `timeout_seconds == 0` 表示不限时，此时连 kill 都不请求。
        let kill_deadline_ms = match timeout_seconds {
            0 => None,
            seconds => self.kill_deadline.deadline_ms(seconds),
        };
        // 客户端整体时限：请求超时 + 备份余量（服务端 deadline 先到，先 kill 进程）。
        let client_deadline = exec_client_deadline(timeout_seconds);
        let call = with_retries(&retry, move || {
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
                if let Some(deadline_ms) = kill_deadline_ms {
                    // envd 读到该头就会在 deadline 到期时 kill 掉远端进程。
                    request = request.header("Connect-Timeout-Ms", deadline_ms.to_string());
                }
                if let Some(token) = &access_token {
                    request = request.header("X-Access-Token", token);
                }
                if let Some(deadline) = client_deadline {
                    // 覆盖客户端级的整体超时（`read_timeout_seconds`），让 exec 时限成为这次
                    // 请求唯一的整体时限（对应 Java 的 per-call `callTimeout`）。
                    request = request.timeout(deadline);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|e| map_exec_transport_error(e, timeout_seconds))?;
                let status = response.status();
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| map_exec_transport_error(e, timeout_seconds))?;
                if !status.is_success() {
                    return Err(SandboxExtensionError::http(
                        status.as_u16(),
                        String::from_utf8_lossy(&bytes).into_owned(),
                    ));
                }
                Ok(bytes.to_vec())
            }
        });
        let body = with_exec_deadline(timeout_seconds, call)
            .await
            .map_err(|err| {
                if matches!(
                    err,
                    SandboxExtensionError::Runtime(SandboxErrorCode::ExecTimeout, _)
                ) {
                    tracing::warn!(
                    timeout_seconds = timeout_seconds,
                    "e2b: exec exceeded its deadline; the remote process was killed server-side"
                );
                }
                err
            })?;
        let stream = match drain_envd_stream(&body) {
            Ok(stream) => stream,
            Err(err) => {
                if matches!(
                    err,
                    SandboxExtensionError::Runtime(SandboxErrorCode::ExecTimeout, _)
                ) {
                    tracing::warn!(
                        timeout_seconds = timeout_seconds,
                        "e2b: exec was killed server-side after the deadline expired"
                    );
                    // 服务端 deadline 到期（envd 已 kill 掉远端进程），统一成 exec 超时错误。
                    return Err(exec_timeout_error(timeout_seconds));
                }
                return Err(err);
            }
        };
        let exit_code = match stream.exit_code {
            Some(code) => code,
            None => {
                // Java：超过 deadline 却没有退出码的流，按 exec 超时处理；deadline 之前则是
                // 普通的流异常。
                if timeout_seconds > 0 && started.elapsed() >= Duration::from_secs(timeout_seconds)
                {
                    return Err(exec_timeout_error(timeout_seconds));
                }
                return Err(SandboxExtensionError::Runtime(
                    SandboxErrorCode::CommandExecutionError,
                    "envd process stream ended before receiving an exit code".to_string(),
                ));
            }
        };
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
                })?
                .to_string();
            self.note_snapshot(&snapshot_id);
            // Best-effort: pruning must never block persistence, and E2B may reject deletes for
            // snapshots still backing a live sandbox (the Java code defers pruning to shutdown
            // for that reason). Failures keep the id recorded for a later attempt.
            if let Err(err) = self.prune_known_snapshots().await {
                tracing::warn!("e2b: snapshot retention pass failed: {err}");
            }
            return Ok(E2bSnapshotRefs::encode_snapshot_id(&snapshot_id));
        }
        self.persist_tar_via_files(handle, root).await
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
                let id = fields.get("sandbox_id").cloned().ok_or_else(|| {
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
        self.hydrate_tar_via_files(handle, root, archive).await?;
        Ok(None)
    }

    async fn file_transfer(
        &self,
        handle: &RemoteSandboxHandle,
    ) -> Option<Arc<dyn SandboxFileTransfer>> {
        Some(Arc::new(E2bFileTransfer::new(self.clone(), handle.clone())))
    }
}

/// De-duplicates snapshot ids, dropping blanks and keeping the first (oldest) occurrence.
///
/// Mirrors the Java `E2bPlatformHttp#dedupe`, so "keep the last N" counts distinct snapshots.
pub fn dedupe_snapshot_ids(snapshot_ids: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(snapshot_ids.len());
    for id in snapshot_ids {
        let trimmed = id.trim();
        if trimmed.is_empty() || out.iter().any(|seen| seen == trimmed) {
            continue;
        }
        out.push(trimmed.to_string());
    }
    out
}

/// Percent-encodes a value for use as a **single** URL path segment.
///
/// Mirrors OkHttp's `HttpUrl.addPathSegment`: `/` becomes `%2F` (so a namespaced snapshot id stays
/// one segment) while the RFC 3986 sub-delims and `:` stay readable, matching the Java test
/// expectation `/templates/team%2Fstale:v1`.
pub fn encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b':'
            | b'@'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'=' => out.push(*byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Builds the envd `Authorization: Basic` header for the run user.
pub fn basic_auth_header(run_user: &str) -> String {
    let user = if run_user.trim().is_empty() {
        "user"
    } else {
        run_user
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:"));
    format!("Basic {encoded}")
}

/// Percent-encodes a query component using the `application/x-www-form-urlencoded` alphabet.
///
/// Matches Java's `URLEncoder.encode(value, UTF_8)`: unreserved `A-Za-z0-9-_.*` pass through and
/// a space becomes `+`; everything else (notably `/` → `%2F`) is percent-encoded byte-wise.
pub fn encode_query_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Extracts the file name from a remote path (Java `filenameFromPath`).
pub fn filename_from_path(path: &str) -> &str {
    match path.rfind('/') {
        Some(idx) => &path[idx + 1..],
        None => path,
    }
}

/// E2B sandbox client (harness [`agentscope_harness::agent::sandbox::SandboxClient`]).
pub type E2bSandboxClient = RemoteSandboxClient;

/// Creates the harness [`agentscope_harness::agent::sandbox::SandboxClient`] for E2B.
pub fn e2b_client(options: E2bSandboxClientOptions) -> SandboxResult<E2bSandboxClient> {
    Ok(RemoteSandboxClient::new(Arc::new(E2bBackend::new(
        options,
    )?)))
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
        assert_eq!(
            fields.get("sandbox_domain").map(String::as_str),
            Some("e2b.dev")
        );
        assert_eq!(
            fields.get("envd_access_token").map(String::as_str),
            Some("tok")
        );
        assert_eq!(
            fields.get("envd_version").map(String::as_str),
            Some("0.1.5")
        );
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
    fn decode_frames_tolerates_fragmented_tail_without_panicking() {
        // A fully decoded frame followed by a frame whose payload has not fully arrived yet:
        // decoding must stop cleanly (no panic, no partial payload) and keep the complete frame.
        let mut data = encode_connect_envelope(b"complete");
        data.extend_from_slice(&[0x00, 0, 0, 0, 10, b'p', b'a', b'r']); // 10 bytes promised, 3 sent
        assert_eq!(decode_connect_frames(&data), vec![b"complete".to_vec()]);

        // Header itself is split across the read boundary (fewer than 5 bytes).
        let partial_header = &encode_connect_envelope(b"x")[..3];
        assert!(decode_connect_frames(partial_header).is_empty());
        assert!(decode_connect_frames(&[]).is_empty());
    }

    #[test]
    fn decode_frames_rejects_oversized_and_reserved_frames() {
        // A length far beyond the 64 MiB guard must stop decoding rather than allocate.
        let mut data = encode_connect_envelope(b"ok");
        data.extend_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(decode_connect_frames(&data), vec![b"ok".to_vec()]);

        // Reserved flag bits (0x04/0x08/...) are not data frames.
        let mut reserved = encode_connect_envelope(b"data");
        reserved.extend_from_slice(&[0x04, 0, 0, 0, 3, b'x', b'y', b'z']);
        assert_eq!(decode_connect_frames(&reserved), vec![b"data".to_vec()]);

        // Envelope decoding preserves the raw flags so callers can distinguish trailers.
        let envelopes = decode_connect_envelopes(&encode_connect_envelope(b"z"));
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].flags, 0x00);
        assert_eq!(envelopes[0].payload, b"z".to_vec());
    }

    #[test]
    fn drain_envd_stream_tolerates_split_frames_and_missing_tail() {
        // Frames arriving across a chunk boundary: stdout then a truncated trailing frame.
        let stdout_b64 = base64::engine::general_purpose::STANDARD.encode(b"partial output");
        let mut body = encode_connect_envelope(
            serde_json::to_vec(&json!({"event":{"data":{"stdout":stdout_b64}}}))
                .unwrap()
                .as_slice(),
        );
        body.extend_from_slice(&[0x00, 0, 0, 0, 30, b'{']); // truncated end frame
        let result = drain_envd_stream(&body).expect("fragmented stream must not fail");
        assert_eq!(result.stdout, b"partial output".to_vec());
        // No exit code: the caller decides how to report an unterminated stream.
        assert_eq!(result.exit_code, None);
    }

    #[test]
    fn drain_envd_stream_skips_corrupt_data_frames() {
        // A corrupt data frame must not abort the stream; the end frame still yields an exit code.
        let mut body = encode_connect_envelope(b"not json at all");
        body.extend_from_slice(&encode_connect_envelope(
            serde_json::to_vec(&json!({"event":{"end":{"exitCode":1}}}))
                .unwrap()
                .as_slice(),
        ));
        let result = drain_envd_stream(&body).unwrap();
        assert_eq!(result.exit_code, Some(1));
    }

    #[test]
    fn drain_envd_stream_rejects_compressed_frames() {
        let mut body = encode_connect_envelope(b"whatever");
        body[0] = CONNECT_FLAG_COMPRESSED;
        let err = drain_envd_stream(&body).unwrap_err();
        assert!(
            matches!(err, SandboxExtensionError::Unsupported(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn drain_envd_stream_propagates_end_stream_error() {
        // flags 0x02: end-stream trailer carrying a Connect Error.
        let trailer = serde_json::to_vec(&json!({
            "error": {"code": "internal", "message": "boom"}
        }))
        .unwrap();
        let mut body = vec![CONNECT_FLAG_END_STREAM];
        body.extend_from_slice(&(trailer.len() as u32).to_be_bytes());
        body.extend_from_slice(&trailer);
        let err = drain_envd_stream(&body).unwrap_err();
        match err {
            SandboxExtensionError::Runtime(code, msg) => {
                assert_eq!(code, SandboxErrorCode::WorkspaceStartError);
                assert!(msg.contains("internal: boom"), "got {msg}");
            }
            other => panic!("expected runtime error, got {other:?}"),
        }

        // A trailer without an error is a clean end of stream.
        let ok = serde_json::to_vec(&json!({})).unwrap();
        let mut clean = vec![CONNECT_FLAG_END_STREAM];
        clean.extend_from_slice(&(ok.len() as u32).to_be_bytes());
        clean.extend_from_slice(&ok);
        assert_eq!(drain_envd_stream(&clean).unwrap().exit_code, None);
    }

    #[test]
    fn parse_end_stream_error_applies_connect_truthiness() {
        assert_eq!(parse_end_stream_error(br#"{}"#), None);
        assert_eq!(parse_end_stream_error(br#"{"error":null}"#), None);
        assert_eq!(parse_end_stream_error(br#"{"error":{}}"#), None);
        assert_eq!(parse_end_stream_error(br#"{"error":""}"#), None);
        assert_eq!(parse_end_stream_error(br#"{"error":0}"#), None);
        assert_eq!(parse_end_stream_error(br#"{"error":false}"#), None);
        assert_eq!(parse_end_stream_error(b"not json"), None);
        assert_eq!(
            parse_end_stream_error(br#"{"error":{"code":"internal","message":"boom"}}"#).as_deref(),
            Some("internal: boom")
        );
        assert_eq!(
            parse_end_stream_error(br#"{"error":{"code":"unavailable"}}"#).as_deref(),
            Some("unavailable")
        );
        assert_eq!(
            parse_end_stream_error(br#"{"error":"plain text"}"#).as_deref(),
            Some(r#""plain text""#)
        );
    }

    #[test]
    fn parse_envd_event_reads_exit_code_and_error() {
        // camelCase exitCode (protojson jsonName) ...
        let event = parse_envd_event(br#"{"event":{"end":{"exitCode":3}}}"#).unwrap();
        assert_eq!(event.exit_code, Some(3));
        // ... and the original snake_case proto field name.
        let snake = parse_envd_event(br#"{"event":{"end":{"exit_code":7}}}"#).unwrap();
        assert_eq!(snake.exit_code, Some(7));
        // exit code 0 must not be misreported as "absent".
        assert_eq!(
            parse_envd_event(br#"{"event":{"end":{"exitCode":0}}}"#)
                .unwrap()
                .exit_code,
            Some(0)
        );

        // EndEvent.error is forwarded to stderr, separated from unterminated stderr output.
        let with_error =
            parse_envd_event(br#"{"event":{"end":{"exitCode":137,"error":"signal: killed"}}}"#)
                .unwrap();
        assert_eq!(with_error.stderr, b"signal: killed".to_vec());
        assert_eq!(with_error.exit_code, Some(137));

        let stderr_b64 = base64::engine::general_purpose::STANDARD.encode(b"oops");
        let combined = parse_envd_event(
            format!(r#"{{"event":{{"data":{{"stderr":"{stderr_b64}"}},"end":{{"exitCode":1,"error":"exit status 1"}}}}}}"#)
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(combined.stderr, b"oops\nexit status 1".to_vec());
    }

    #[test]
    fn resolve_envd_base_url_prefers_option_then_env_then_host() {
        assert_eq!(
            resolve_envd_base_url(
                Some("http://10.0.0.1:8080"),
                Some("http://env:1"),
                "sbx",
                "e2b.app"
            ),
            "http://10.0.0.1:8080"
        );
        assert_eq!(
            resolve_envd_base_url(None, Some("http://env:1/"), "sbx", "e2b.app"),
            "http://env:1"
        );
        // Blank values are ignored at every level.
        assert_eq!(
            resolve_envd_base_url(Some("  "), Some("   "), "sbx", "e2b.app"),
            "https://49983-sbx.e2b.app"
        );
        assert_eq!(
            resolve_envd_base_url(None, None, "sbx", "e2b.app"),
            "https://49983-sbx.e2b.app"
        );
        // Trailing slash trimming matches the Java `E2bPlatformHttp.trimSlash`.
        assert_eq!(
            resolve_envd_base_url(Some("http://gateway.local:9000///"), None, "sbx", "e2b.app"),
            "http://gateway.local:9000"
        );
    }

    #[test]
    fn envd_sandbox_url_reads_from_injected_env_lookup() {
        assert_eq!(envd_sandbox_url_from(|_| None), None);
        assert_eq!(envd_sandbox_url_from(|_| Some("  ".to_string())), None);
        assert_eq!(
            envd_sandbox_url_from(|key| {
                assert_eq!(key, E2B_SANDBOX_URL_ENV);
                Some(" http://cube.local:8080/ ".to_string())
            })
            .as_deref(),
            Some("http://cube.local:8080")
        );
        assert_eq!(E2B_SANDBOX_URL_ENV, "E2B_SANDBOX_URL");
    }

    #[test]
    fn encode_query_component_matches_java_url_encoder() {
        // Java: URLEncoder.encode("/tmp/test.txt", UTF_8) == "%2Ftmp%2Ftest.txt"
        assert_eq!(encode_query_component("/tmp/test.txt"), "%2Ftmp%2Ftest.txt");
        assert_eq!(encode_query_component("user"), "user");
        assert_eq!(encode_query_component("a b"), "a+b");
        assert_eq!(encode_query_component("a-b_c.d*e"), "a-b_c.d*e");
        assert_eq!(encode_query_component("a=b&c"), "a%3Db%26c");
    }

    #[test]
    fn filename_from_path_extracts_basename() {
        assert_eq!(filename_from_path("/tmp/test.txt"), "test.txt");
        assert_eq!(filename_from_path("relative.md"), "relative.md");
        assert_eq!(filename_from_path("/tmp/dir/"), "");
    }

    #[test]
    fn drain_envd_stream_aggregates_events() {
        let stdout_b64 = base64::engine::general_purpose::STANDARD.encode(b"hello ");
        let stderr_b64 = base64::engine::general_purpose::STANDARD.encode(b"warn");
        let mut body = Vec::new();
        body.extend_from_slice(&encode_connect_envelope(
            serde_json::to_vec(&json!({"event":{"data":{"stdout":stdout_b64}}}))
                .unwrap()
                .as_slice(),
        ));
        body.extend_from_slice(&encode_connect_envelope(
            serde_json::to_vec(&json!({"event":{"data":{"stdout":base64::engine::general_purpose::STANDARD.encode(b"world"),"stderr":stderr_b64}}})).unwrap().as_slice(),
        ));
        body.extend_from_slice(&encode_connect_envelope(
            serde_json::to_vec(&json!({"event":{"end":{"exitCode":0}}}))
                .unwrap()
                .as_slice(),
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
        assert_eq!(
            E2bSnapshotRefs::decode_snapshot_id_if_present(b"plain tar bytes"),
            None
        );
        assert_eq!(
            E2bSnapshotRefs::decode_snapshot_id_if_present(b"E2B_SANDBOX_SNAPSHOT_V1\n"),
            None
        );
    }

    #[test]
    fn persistence_mode_and_codec_parse() {
        assert_eq!(
            E2bPersistenceMode::parse("NATIVE_SNAPSHOT"),
            E2bPersistenceMode::NativeSnapshot
        );
        assert_eq!(E2bPersistenceMode::parse("bogus"), E2bPersistenceMode::Tar);
        assert_eq!(E2bCodec::parse("JSON"), E2bCodec::Json);
        assert_eq!(E2bCodec::parse("PROTO"), E2bCodec::Proto);
        // The default codec must be the one this crate can actually encode (see `E2bCodec`).
        assert_eq!(E2bCodec::parse("bogus"), E2bCodec::Json);
        assert_eq!(E2bCodec::default(), E2bCodec::Json);
        assert_eq!(E2bSandboxClientOptions::default().codec, E2bCodec::Json);
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
            codec: E2bCodec::Proto,
            ..Default::default()
        })
        .unwrap();
        let handle = RemoteSandboxHandle::new("sbx", BTreeMap::new());
        let err = backend
            .exec_in(&handle, "echo hi", None, 5)
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxExtensionError::Unsupported(_)));
    }

    #[tokio::test]
    async fn default_codec_exec_works_without_opt_in() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let body = server
            .mock_async(|when, then| {
                when.method(POST).path("/process.Process/Start");
                then.status(200)
                    .header("content-type", "application/connect+json")
                    .body(envd_body_with_exit(0, b"hi"));
            })
            .await;
        // Only the API key is configured: codec, timeouts and everything else stay at defaults.
        let backend = E2bBackend::new(E2bSandboxClientOptions {
            api_key: Some("k".to_string()),
            envd_sandbox_url: Some(server.base_url()),
            max_retries: 1,
            ..Default::default()
        })
        .unwrap();
        let handle = RemoteSandboxHandle::new("sbx-1", BTreeMap::new());
        let output = backend
            .exec_in(&handle, "echo hi", None, 5)
            .await
            .expect("the default codec must be usable");
        body.assert_async().await;
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, "hi");
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
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/sandboxes")
                    .header("x-api-key", "secret")
                    .json_body(json!({"templateID":"base","timeout":300}));
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"sandboxID":"sbx-9","domain":"e2b.dev","envdAccessToken":"tok"}"#);
            })
            .await;
        let backend = E2bBackend::new(E2bSandboxClientOptions {
            api_key: Some("secret".to_string()),
            api_base_url: server.base_url(),
            ..Default::default()
        })
        .unwrap();
        let handle = backend.create_sandbox("sess-1").await.unwrap();
        mock.assert_async().await;
        assert_eq!(handle.id, "sbx-9");
        assert_eq!(
            handle.extra.get("sandbox_domain").map(String::as_str),
            Some("e2b.dev")
        );
        assert_eq!(
            handle.extra.get("persistence_mode").map(String::as_str),
            Some("TAR")
        );
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
            snapshot: None,
        };
        assert_eq!(data.to_harness_state().session_id(), "sess-1");
    }

    fn http_backend(api_base_url: String, envd_sandbox_url: Option<String>) -> E2bBackend {
        E2bBackend::new(E2bSandboxClientOptions {
            api_key: Some("secret".to_string()),
            api_base_url,
            envd_sandbox_url,
            codec: E2bCodec::Json,
            max_retries: 1,
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn upload_file_posts_multipart_to_envd_files() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/files")
                    .query_param("path", "/tmp/test.txt")
                    .query_param("username", "user")
                    .header("e2b-sandbox-id", "sbx-1")
                    .header("e2b-sandbox-port", "49983")
                    .header("authorization", basic_auth_header("user"))
                    .body_contains("hello world");
                then.status(200).body("");
            })
            .await;
        let backend = http_backend(server.base_url(), Some(server.base_url()));
        let mut extra = BTreeMap::new();
        extra.insert("sandbox_domain".to_string(), "e2b.dev".to_string());
        let handle = RemoteSandboxHandle::new("sbx-1", extra);

        backend
            .upload_file(&handle, "/tmp/test.txt", b"hello world")
            .await
            .expect("upload succeeds");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn upload_file_rejects_directory_path() {
        let backend = http_backend("http://127.0.0.1:1".to_string(), None);
        let handle = RemoteSandboxHandle::new("sbx-1", BTreeMap::new());
        let err = backend
            .upload_file(&handle, "/tmp/dir/", b"x")
            .await
            .unwrap_err();
        assert!(
            matches!(err, SandboxExtensionError::Configuration(_)),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn download_file_gets_from_envd_files() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/files")
                    .query_param("path", "/home/user/a.txt")
                    .query_param("username", "user")
                    .header("e2b-sandbox-id", "sbx-1")
                    .header("x-access-token", "tok");
                then.status(200).body("file-bytes");
            })
            .await;
        let backend = http_backend(server.base_url(), Some(server.base_url()));
        let mut extra = BTreeMap::new();
        extra.insert("envd_access_token".to_string(), "tok".to_string());
        let handle = RemoteSandboxHandle::new("sbx-1", extra);

        let bytes = backend
            .download_file(&handle, "/home/user/a.txt")
            .await
            .expect("download succeeds");
        assert_eq!(bytes, b"file-bytes".to_vec());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn download_file_reports_archive_read_error_on_failure() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET).path("/files");
                then.status(404).body("missing");
            })
            .await;
        let backend = http_backend(server.base_url(), Some(server.base_url()));
        let handle = RemoteSandboxHandle::new("sbx-1", BTreeMap::new());

        let err = backend
            .download_file(&handle, "/home/user/a.txt")
            .await
            .unwrap_err();
        match err {
            SandboxExtensionError::Runtime(code, msg) => {
                assert_eq!(code, SandboxErrorCode::WorkspaceArchiveReadError);
                assert!(msg.contains("404"), "got {msg}");
            }
            other => panic!("expected runtime error, got {other:?}"),
        }
    }

    // ---------------------------------- native file transfer (`SandboxFileTransfer`)

    #[test]
    fn workspace_transfer_path_stays_inside_the_workspace() {
        assert_eq!(
            workspace_transfer_path("/home/user", "/home/user/a.txt").as_deref(),
            Some("/home/user/a.txt")
        );
        assert_eq!(
            workspace_transfer_path("/home/user/", "/home/user/sub/a.txt").as_deref(),
            Some("/home/user/sub/a.txt")
        );
        // The workspace root itself, siblings that only share a prefix, relative paths and
        // traversal segments are all declined (base64-over-exec handles them).
        assert_eq!(workspace_transfer_path("/home/user", "/home/user"), None);
        assert_eq!(
            workspace_transfer_path("/home/user", "/home/user-x/a.txt"),
            None
        );
        assert_eq!(workspace_transfer_path("/home/user", "/etc/hosts"), None);
        assert_eq!(workspace_transfer_path("/home/user", "a.txt"), None);
        assert_eq!(
            workspace_transfer_path("/home/user", "/home/user/../etc/passwd"),
            None
        );
        assert_eq!(
            workspace_transfer_path("/home/user", "/home/user/a//b"),
            None
        );
        assert_eq!(
            workspace_transfer_path("/home/user", "/home/user/./a"),
            None
        );
        assert_eq!(workspace_transfer_path("", "/home/user/a.txt"), None);
        // A root-level workspace accepts everything beneath `/`.
        assert_eq!(
            workspace_transfer_path("/", "/a.txt").as_deref(),
            Some("/a.txt")
        );
    }

    #[test]
    fn file_transfer_declines_paths_outside_the_workspace() {
        let backend = E2bBackend::new(E2bSandboxClientOptions::default()).unwrap();
        let transfer =
            E2bFileTransfer::new(backend, RemoteSandboxHandle::new("sbx-1", BTreeMap::new()));
        assert!(transfer.supports_file_transfer("/home/user/a.txt"));
        assert!(!transfer.supports_file_transfer("/tmp/a.txt"));
    }

    #[tokio::test]
    async fn file_transfer_uploads_and_downloads_through_envd_files() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let upload = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/files")
                    .query_param("path", "/home/user/a.txt")
                    .body_contains("hello");
                then.status(200).body("");
            })
            .await;
        let download = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/files")
                    .query_param("path", "/home/user/a.txt");
                then.status(200).body("world");
            })
            .await;
        let backend = http_backend(server.base_url(), Some(server.base_url()));
        let transfer =
            E2bFileTransfer::new(backend, RemoteSandboxHandle::new("sbx-1", BTreeMap::new()));

        transfer
            .upload_file("/home/user/a.txt", b"hello")
            .await
            .expect("native upload succeeds");
        upload.assert_async().await;
        assert_eq!(
            transfer
                .download_file("/home/user/a.txt")
                .await
                .expect("native download succeeds"),
            b"world".to_vec()
        );
        download.assert_async().await;

        // Out-of-workspace paths are rejected instead of silently escaping.
        assert!(transfer.upload_file("/etc/passwd", b"x").await.is_err());
    }

    #[tokio::test]
    async fn persist_tar_uses_envd_files_instead_of_base64_over_exec() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        // The base64-over-exec strategy must not be taken at all.
        let base64_exec = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/process.Process/Start")
                    .body_contains("base64");
                then.status(200).body(envd_body_with_exit(0, b""));
            })
            .await;
        let exec = server
            .mock_async(|when, then| {
                when.method(POST).path("/process.Process/Start");
                then.status(200).body(envd_body_with_exit(0, b""));
            })
            .await;
        let download = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/files")
                    .query_param_exists("path")
                    .query_param("username", "user");
                then.status(200).body("TARDATA");
            })
            .await;
        let backend = http_backend(server.base_url(), Some(server.base_url()));
        let handle = RemoteSandboxHandle::new("sbx-1", BTreeMap::new());

        let archive = backend
            .persist_workspace_bytes(&handle, "/home/user")
            .await
            .expect("tar persist succeeds");
        assert_eq!(archive, b"TARDATA".to_vec());
        assert_eq!(base64_exec.hits_async().await, 0);
        // `tar` + `rm -f` both go through the exec channel.
        assert_eq!(exec.hits_async().await, 2);
        assert!(download.hits_async().await >= 1);
    }

    #[tokio::test]
    async fn hydrate_tar_uploads_the_archive_before_extracting() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let upload = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/files")
                    .query_param_exists("path")
                    .body_contains("TARDATA");
                then.status(200).body("");
            })
            .await;
        let exec = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/process.Process/Start")
                    .body_contains("tar -xf");
                then.status(200).body(envd_body_with_exit(0, b""));
            })
            .await;
        let backend = http_backend(server.base_url(), Some(server.base_url()));
        let handle = RemoteSandboxHandle::new("sbx-1", BTreeMap::new());

        backend
            .hydrate_workspace_bytes(&handle, "/home/user", b"TARDATA")
            .await
            .expect("tar hydrate succeeds");
        upload.assert_async().await;
        exec.assert_async().await;
    }

    #[tokio::test]
    async fn envd_sandbox_url_option_routes_exec_through_gateway() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        // The mock only answers on the gateway path; a wildcard-DNS host would never reach it.
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/process.Process/Start")
                    .header("e2b-sandbox-id", "sbx-1")
                    .header("e2b-sandbox-port", "49983")
                    .header("content-type", "application/connect+json");
                then.status(200).body(envd_body_with_exit(0, b"hi"));
            })
            .await;
        let backend = http_backend("http://127.0.0.1:1".to_string(), Some(server.base_url()));
        let handle = RemoteSandboxHandle::new("sbx-1", BTreeMap::new());

        let output = backend.exec_in(&handle, "echo hi", None, 5).await.unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, "hi");
        mock.assert_async().await;
    }

    /// Builds a Connect response body carrying one stdout event and one end event.
    fn envd_body_with_exit(exit_code: i32, stdout: &[u8]) -> Vec<u8> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(stdout);
        let mut body = encode_connect_envelope(
            serde_json::to_vec(&json!({"event":{"data":{"stdout":encoded}}}))
                .unwrap()
                .as_slice(),
        );
        body.extend_from_slice(&encode_connect_envelope(
            serde_json::to_vec(&json!({"event":{"end":{"exitCode":exit_code}}}))
                .unwrap()
                .as_slice(),
        ));
        body
    }

    // ---------------------------------------------------------------- snapshot pruning (GAP-11)

    fn retention_backend(server: &httpmock::MockServer, retention: Option<usize>) -> E2bBackend {
        E2bBackend::new(E2bSandboxClientOptions {
            api_key: Some("secret".to_string()),
            api_base_url: server.base_url(),
            snapshot_retention: retention,
            persistence_mode: E2bPersistenceMode::NativeSnapshot,
            max_retries: 1,
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn encode_path_segment_matches_okhttp_add_path_segment() {
        assert_eq!(encode_path_segment("snap-1"), "snap-1");
        // A namespaced id must stay one segment, otherwise E2B parses it as an extra path part.
        assert_eq!(encode_path_segment("team/stale:v1"), "team%2Fstale:v1");
        assert_eq!(encode_path_segment("a b"), "a%20b");
        assert_eq!(encode_path_segment("q?x#y"), "q%3Fx%23y");
    }

    #[test]
    fn dedupe_snapshot_ids_drops_blanks_and_keeps_first_occurrence() {
        let input = vec![
            "snap-1".to_string(),
            "  ".to_string(),
            "snap-2".to_string(),
            "snap-1".to_string(),
            "snap-3".to_string(),
        ];
        assert_eq!(
            dedupe_snapshot_ids(&input),
            vec![
                "snap-1".to_string(),
                "snap-2".to_string(),
                "snap-3".to_string()
            ]
        );
        assert!(dedupe_snapshot_ids(&[]).is_empty());
    }

    /// (a) `DELETE /templates/{snapshotId}` — method, encoded path and API-key header.
    #[tokio::test]
    async fn delete_snapshot_sends_delete_to_templates_endpoint() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(DELETE)
                    .path("/templates/team%2Falias:tag")
                    .header("x-api-key", "secret");
                then.status(204).body("");
            })
            .await;
        let backend = retention_backend(&server, None);

        backend
            .delete_snapshot("team/alias:tag")
            .await
            .expect("delete succeeds");
        mock.assert_async().await;
    }

    /// (b) A 404 must surface as a diagnosable error rather than a panic or silent success.
    #[tokio::test]
    async fn delete_snapshot_404_returns_diagnostic_error() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(DELETE).path("/templates/snap-gone");
                then.status(404).body("snapshot not found");
            })
            .await;
        let backend = retention_backend(&server, None);

        let err = backend.delete_snapshot("snap-gone").await.unwrap_err();
        assert_eq!(err.status(), Some(404), "got {err:?}");
        let message = err.to_string();
        assert!(message.contains("404"), "got {message}");
        assert!(message.contains("snap-gone"), "got {message}");
        assert!(message.contains("not found"), "got {message}");
        assert!(message.contains("snapshot not found"), "got {message}");
    }

    #[tokio::test]
    async fn delete_snapshot_401_points_at_the_api_key_option() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(DELETE).path("/templates/snap-1");
                then.status(401).body("invalid api key");
            })
            .await;
        let backend = retention_backend(&server, None);

        let err = backend.delete_snapshot("snap-1").await.unwrap_err();
        assert_eq!(err.status(), Some(401), "got {err:?}");
        let message = err.to_string();
        assert!(message.contains("401"), "got {message}");
        assert!(message.contains("api_key"), "got {message}");
        assert!(message.contains("snap-1"), "got {message}");
    }

    #[tokio::test]
    async fn delete_snapshot_rejects_blank_id_without_traffic() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(DELETE);
                then.status(200);
            })
            .await;
        let backend = retention_backend(&server, None);

        let err = backend.delete_snapshot("   ").await.unwrap_err();
        assert!(
            matches!(err, SandboxExtensionError::Configuration(_)),
            "got {err:?}"
        );
        assert_eq!(mock.hits_async().await, 0);
    }

    /// (c) N=2 with three known snapshots must delete exactly the oldest one.
    #[tokio::test]
    async fn snapshot_retention_keeps_last_n_and_prunes_only_the_oldest() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let oldest = server
            .mock_async(|when, then| {
                when.method(DELETE).path("/templates/snap-1");
                then.status(200).body("");
            })
            .await;
        let newer = server
            .mock_async(|when, then| {
                when.method(DELETE).path("/templates/snap-2");
                then.status(200).body("");
            })
            .await;
        let newest = server
            .mock_async(|when, then| {
                when.method(DELETE).path("/templates/snap-3");
                then.status(200).body("");
            })
            .await;
        let backend = retention_backend(&server, Some(2));
        backend.note_snapshot("snap-1");
        backend.note_snapshot("snap-2");
        backend.note_snapshot("snap-3");

        backend
            .prune_known_snapshots()
            .await
            .expect("prune succeeds");

        assert_eq!(
            oldest.hits_async().await,
            1,
            "oldest snapshot must be deleted once"
        );
        assert_eq!(newer.hits_async().await, 0, "snap-2 is inside the window");
        assert_eq!(newest.hits_async().await, 0, "snap-3 is inside the window");
        assert_eq!(
            backend.known_snapshot_ids(),
            vec!["snap-2".to_string(), "snap-3".to_string()]
        );
    }

    /// (d) Retention disabled (unset / 0) must not issue any delete request.
    #[tokio::test]
    async fn snapshot_retention_disabled_makes_no_delete_request() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(DELETE);
                then.status(200);
            })
            .await;

        for retention in [None, Some(0)] {
            let backend = retention_backend(&server, retention);
            backend.note_snapshot("snap-1");
            backend.note_snapshot("snap-2");
            backend.note_snapshot("snap-3");
            backend
                .prune_known_snapshots()
                .await
                .expect("no-op succeeds");
            assert_eq!(
                backend.known_snapshot_ids(),
                vec![
                    "snap-1".to_string(),
                    "snap-2".to_string(),
                    "snap-3".to_string()
                ],
                "retention {retention:?} must retain every id"
            );
        }
        assert_eq!(
            mock.hits_async().await,
            0,
            "disabled retention must stay offline"
        );
    }

    #[tokio::test]
    async fn snapshot_retention_404_counts_as_already_pruned() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(DELETE).path("/templates/snap-1");
                then.status(404).body("gone");
            })
            .await;
        let backend = retention_backend(&server, Some(1));
        backend.note_snapshot("snap-1");
        backend.note_snapshot("snap-2");

        let kept = backend
            .prune_snapshot_ids(&backend.known_snapshot_ids(), 1)
            .await;
        assert_eq!(
            kept,
            vec!["snap-2".to_string()],
            "a 404 must not be retried forever"
        );
    }

    #[tokio::test]
    async fn snapshot_retention_keeps_ids_whose_delete_failed() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(DELETE).path("/templates/snap-1");
                then.status(500).body("locked");
            })
            .await;
        let backend = retention_backend(&server, Some(1));
        backend.note_snapshot("snap-1");
        backend.note_snapshot("snap-2");

        backend
            .prune_known_snapshots()
            .await
            .expect("prune is best effort");
        assert_eq!(
            backend.known_snapshot_ids(),
            vec!["snap-1".to_string(), "snap-2".to_string()],
            "a failed delete stays recorded for a later attempt"
        );
    }

    /// Persisting a native snapshot records the id, then applies the window to it.
    #[tokio::test]
    async fn persist_native_snapshot_records_id_and_applies_retention() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let create = server
            .mock_async(|when, then| {
                when.method(POST).path("/sandboxes/sbx-1/snapshots");
                then.status(200)
                    .header("content-type", "application/json")
                    .body("{\"snapshotID\":\"snap-1\"}");
            })
            .await;
        let prune = server
            .mock_async(|when, then| {
                when.method(DELETE).path("/templates/snap-0");
                then.status(200).body("");
            })
            .await;
        let keep = server
            .mock_async(|when, then| {
                when.method(DELETE).path("/templates/snap-1");
                then.status(200).body("");
            })
            .await;
        let backend = retention_backend(&server, Some(1));
        backend.note_snapshot("snap-0");
        let handle = RemoteSandboxHandle::new("sbx-1", BTreeMap::new());

        let archive = backend
            .persist_workspace_bytes(&handle, "/home/user")
            .await
            .expect("native snapshot persists");
        assert_eq!(
            E2bSnapshotRefs::decode_snapshot_id_if_present(&archive).as_deref(),
            Some("snap-1")
        );

        create.assert_async().await;
        prune.assert_async().await;
        assert_eq!(
            keep.hits_async().await,
            0,
            "the newest snapshot must survive"
        );
        assert_eq!(backend.known_snapshot_ids(), vec!["snap-1".to_string()]);
    }

    // ------------------------------------------------------ volume mounts (E2B `volumeMounts`)

    #[test]
    fn volume_mount_validates_name_and_normalizes_path() {
        let mount = E2bVolumeMount::new("vol-a", "/mnt/data/").expect("valid mount");
        assert_eq!(mount.name, "vol-a");
        assert_eq!(mount.path, "/mnt/data");
        // 空白名 / 空白路径 / 相对路径都是配置错误。
        assert!(E2bVolumeMount::new("  ", "/mnt/data").is_err());
        assert!(E2bVolumeMount::new("vol-a", "   ").is_err());
        assert!(E2bVolumeMount::new("vol-a", "mnt/data").is_err());
        // 反斜杠与重复斜杠被折叠，结尾斜杠被去掉（根路径除外）。
        assert_eq!(
            E2bVolumeMount::normalize_mount_path("//mnt\\\\data//").unwrap(),
            "/mnt/data"
        );
        assert_eq!(E2bVolumeMount::normalize_mount_path("/").unwrap(), "/");
    }

    #[test]
    fn volume_mount_covers_workspace_root() {
        assert!(E2bVolumeMount::covers_workspace_root(
            "/mnt/data",
            "/mnt/data"
        ));
        assert!(E2bVolumeMount::covers_workspace_root(
            "/mnt/data/sub",
            "/mnt/data"
        ));
        // `/mnt/data-x` 不在 `/mnt/data` 挂载下。
        assert!(!E2bVolumeMount::covers_workspace_root(
            "/mnt/data-x",
            "/mnt/data"
        ));
        assert!(E2bVolumeMount::covers_workspace_root("/anything", "/"));
        assert!(!E2bVolumeMount::covers_workspace_root("", "/mnt/data"));
    }

    #[test]
    fn require_distinct_mount_paths_rejects_duplicates() {
        let a = E2bVolumeMount::new("vol-a", "/mnt/data").unwrap();
        let b = E2bVolumeMount::new("vol-b", "/mnt/data").unwrap();
        let c = E2bVolumeMount::new("vol-c", "/mnt/other").unwrap();
        assert!(require_distinct_mount_paths(&[a.clone(), c.clone()]).is_ok());
        let err = require_distinct_mount_paths(&[a, b]).unwrap_err();
        assert!(err.to_string().contains("/mnt/data"), "got {err}");
    }

    #[test]
    fn options_with_volume_mount_validates_and_rejects_duplicates() {
        let options = E2bSandboxClientOptions::default()
            .with_volume_mount("vol-a", "/mnt/data")
            .expect("valid mount");
        assert_eq!(options.volume_mounts.len(), 1);
        assert_eq!(options.volume_mounts[0].name, "vol-a");
        assert_eq!(options.volume_mounts[0].path, "/mnt/data");
        // 路径重复必须被拒绝（Java `setVolumeMounts` 的 distinct-path 检查）。
        let err = options.with_volume_mount("vol-b", "/mnt/data").unwrap_err();
        assert!(err.to_string().contains("distinct"), "got {err}");
        // 非法挂载直接失败，不进入配置。
        assert!(E2bSandboxClientOptions::default()
            .with_volume_mount("vol-a", "relative/path")
            .is_err());
    }

    #[test]
    fn create_body_with_volume_mounts_matches_java() {
        let mount = E2bVolumeMount::new("vol-a", "/mnt/data").unwrap();
        let body = build_create_sandbox_body_with_mounts("base", 300, &[mount]);
        assert_eq!(body["templateID"], "base");
        assert_eq!(body["timeout"], 300);
        let mounts = body["volumeMounts"].as_array().expect("volumeMounts array");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0]["name"], "vol-a");
        assert_eq!(mounts[0]["path"], "/mnt/data");
    }

    #[test]
    fn create_body_without_volume_mounts_keeps_the_legacy_shape() {
        // 无挂载时请求体必须和旧形状完全一致（不能塞空数组）。
        assert_eq!(
            build_create_sandbox_body("base", 300),
            json!({ "templateID": "base", "timeout": 300 })
        );
        assert_eq!(
            build_create_sandbox_body_with_mounts("base", 300, &[]),
            json!({ "templateID": "base", "timeout": 300 })
        );
        assert!(build_create_sandbox_body("base", 300)
            .get("volumeMounts")
            .is_none());
    }

    #[tokio::test]
    async fn platform_create_sends_volume_mounts() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST).path("/sandboxes").json_body(json!({
                    "templateID": "base",
                    "timeout": 300,
                    "volumeMounts": [{"name": "vol-a", "path": "/mnt/data"}],
                }));
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"sandboxID":"sbx-10","domain":"e2b.dev"}"#);
            })
            .await;
        let backend = E2bBackend::new(E2bSandboxClientOptions {
            api_key: Some("secret".to_string()),
            api_base_url: server.base_url(),
            volume_mounts: vec![E2bVolumeMount::new("vol-a", "/mnt/data").unwrap()],
            ..Default::default()
        })
        .unwrap();

        let handle = backend.create_sandbox("sess-1").await.unwrap();
        mock.assert_async().await;
        assert_eq!(handle.id, "sbx-10");
    }

    // --------------------------------------------- exec deadline + server-side kill

    /// Recording fake for the server-side kill channel.
    #[derive(Debug, Default)]
    struct RecordingKillDeadline {
        calls: Mutex<Vec<u64>>,
    }

    impl RecordingKillDeadline {
        /// The requested deadlines, in call order.
        fn calls(&self) -> Vec<u64> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl E2bKillDeadline for RecordingKillDeadline {
        fn deadline_ms(&self, timeout_seconds: u64) -> Option<u64> {
            self.calls.lock().unwrap().push(timeout_seconds);
            Some(timeout_seconds.saturating_mul(1000))
        }
    }

    /// Builds a Connect response body carrying a server-side deadline kill.
    fn envd_body_with_deadline_exceeded() -> Vec<u8> {
        let trailer = serde_json::to_vec(&json!({
            "error": {"code": "deadline_exceeded", "message": "deadline exceeded"}
        }))
        .unwrap();
        let mut body = vec![CONNECT_FLAG_END_STREAM];
        body.extend_from_slice(&(trailer.len() as u32).to_be_bytes());
        body.extend_from_slice(&trailer);
        body
    }

    /// httpmock matcher: the request must **not** carry the Connect kill deadline header.
    fn without_connect_timeout_header(req: &httpmock::prelude::HttpMockRequest) -> bool {
        match &req.headers {
            None => true,
            Some(headers) => !headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("connect-timeout-ms")),
        }
    }

    #[test]
    fn connect_timeout_ms_header_arms_the_server_side_kill() {
        assert_eq!(connect_timeout_ms_header(5).as_deref(), Some("5000"));
        assert_eq!(connect_timeout_ms_header(120).as_deref(), Some("120000"));
        // 0 表示不限时：不能发 `Connect-Timeout-Ms: 0`（那是"deadline 已过期"）。
        assert_eq!(connect_timeout_ms_header(0), None);
        assert_eq!(EnvdKillDeadline.deadline_ms(0), None);
        assert_eq!(EnvdKillDeadline.deadline_ms(3), Some(3000));
    }

    #[test]
    fn exec_client_deadline_adds_the_backup_slack() {
        // 不限时时不额外加限时，transport 的 read_timeout_seconds 仍是唯一上限。
        assert_eq!(exec_client_deadline(0), None);
        assert_eq!(
            exec_client_deadline(5),
            Some(Duration::from_secs(5 + CLIENT_TIMEOUT_SLACK_SECONDS))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn exec_deadline_expires_and_reports_a_timeout() {
        let result: SandboxResult<()> = with_exec_deadline(1, async {
            std::future::pending::<()>().await;
            Ok::<(), SandboxExtensionError>(())
        })
        .await;
        let err = result.unwrap_err();
        match err {
            SandboxExtensionError::Runtime(code, msg) => {
                assert_eq!(code, SandboxErrorCode::ExecTimeout);
                assert!(msg.contains("timed out after 1s"), "got {msg}");
                // 错误信息不能带命令文本（命令是模型生成的，可能内联密钥）。
                assert!(!msg.contains("sleep"), "got {msg}");
            }
            other => panic!("expected an exec timeout error, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn exec_without_deadline_is_not_limited() {
        let result: SandboxResult<u32> =
            with_exec_deadline(0, async { Ok::<u32, SandboxExtensionError>(7) }).await;
        assert_eq!(result.unwrap(), 7);
    }

    #[tokio::test]
    async fn exec_timeout_arms_the_server_side_kill_and_reports_an_error() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/process.Process/Start")
                    // 服务端 kill 的 deadline 必须真的发出去。
                    .header("connect-timeout-ms", "5000");
                then.status(200).body(envd_body_with_deadline_exceeded());
            })
            .await;
        let recorder = Arc::new(RecordingKillDeadline::default());
        let killer: Arc<dyn E2bKillDeadline> = Arc::clone(&recorder) as Arc<dyn E2bKillDeadline>;
        let backend = E2bBackend::with_kill_deadline(
            E2bSandboxClientOptions {
                api_key: Some("secret".to_string()),
                api_base_url: "http://127.0.0.1:1".to_string(),
                envd_sandbox_url: Some(server.base_url()),
                codec: E2bCodec::Json,
                max_retries: 1,
                ..Default::default()
            },
            killer,
        )
        .unwrap();
        let handle = RemoteSandboxHandle::new("sbx-1", BTreeMap::new());

        let err = backend
            .exec_in(&handle, "sleep 100", None, 5)
            .await
            .unwrap_err();
        match err {
            SandboxExtensionError::Runtime(code, msg) => {
                assert_eq!(code, SandboxErrorCode::ExecTimeout);
                assert!(msg.contains("timed out after 5s"), "got {msg}");
            }
            other => panic!("expected an exec timeout error, got {other:?}"),
        }
        // kill 确实被请求过，且参数就是请求的 timeout。
        assert_eq!(recorder.calls(), vec![5]);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn exec_without_deadline_does_not_arm_the_kill() {
        use httpmock::prelude::*;
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/process.Process/Start")
                    .matches(without_connect_timeout_header);
                then.status(200).body(envd_body_with_exit(0, b"hi"));
            })
            .await;
        let recorder = Arc::new(RecordingKillDeadline::default());
        let killer: Arc<dyn E2bKillDeadline> = Arc::clone(&recorder) as Arc<dyn E2bKillDeadline>;
        let backend = E2bBackend::with_kill_deadline(
            E2bSandboxClientOptions {
                api_key: Some("secret".to_string()),
                api_base_url: "http://127.0.0.1:1".to_string(),
                envd_sandbox_url: Some(server.base_url()),
                codec: E2bCodec::Json,
                max_retries: 1,
                ..Default::default()
            },
            killer,
        )
        .unwrap();
        let handle = RemoteSandboxHandle::new("sbx-1", BTreeMap::new());

        let output = backend
            .exec_in(&handle, "echo hi", None, 0)
            .await
            .expect("exec succeeds");
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, "hi");
        // 未限时 => 没有请求任何 kill。
        assert!(recorder.calls().is_empty());
        mock.assert_async().await;
    }

    #[test]
    fn drain_envd_stream_maps_deadline_exceeded_to_an_exec_timeout() {
        let err = drain_envd_stream(&envd_body_with_deadline_exceeded()).unwrap_err();
        match err {
            SandboxExtensionError::Runtime(code, msg) => {
                assert_eq!(code, SandboxErrorCode::ExecTimeout);
                assert!(msg.contains("deadline_exceeded"), "got {msg}");
            }
            other => panic!("expected an exec timeout error, got {other:?}"),
        }
    }
}
