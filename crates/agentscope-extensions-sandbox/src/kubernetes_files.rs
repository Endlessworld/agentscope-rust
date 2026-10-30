//! Kubernetes filesystem API and `SandboxFileTransfer` adapter.
//!
//! Mirrors the Java `io.agentscope.extensions.sandbox.kubernetes.client.Filesystem` +
//! `client/model/FileEntry` and the `KubernetesSandbox`'s `SandboxFileTransfer` implementation.
//!
//! The sandbox runtime exposes a small HTTP file API rooted at a configured base directory
//! (`upload` / `download/{path}` / `list/{path}` / `exists/{path}`, paths percent-encoded).
//! Every call goes through the injectable [`FileApiTransport`] seam so the request bodies and
//! response parsing can be unit-tested offline.
//!
//! The harness has no `SandboxFileTransfer` trait (Rust's `SandboxFilesystem` is command-based
//! only), so the file-transfer contract is declared **in this crate** as [`SandboxFileTransfer`]
//! and implemented by [`KubernetesFileTransfer`]. This is recorded as a harness gap in the
//! module tracker.

use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::error::{SandboxErrorCode, SandboxExtensionError, SandboxResult};
use crate::http::join_url;
use crate::remote::RemoteExecOutput;

/// Runtime file-API endpoint for uploads
pub const FILE_API_UPLOAD_PATH: &str = "upload";
/// Runtime file-API endpoint prefix for downloads.
pub const FILE_API_DOWNLOAD_PREFIX: &str = "download";
/// Runtime file-API endpoint prefix for directory listings.
pub const FILE_API_LIST_PREFIX: &str = "list";
/// Runtime file-API endpoint prefix for existence checks.
pub const FILE_API_EXISTS_PREFIX: &str = "exists";
/// Relative temp directory used for archive round-trips under the file-API base dir.
pub const FILE_API_TMP_DIR_REL: &str = ".agentscope-tmp";
/// Maximum body length echoed back in a file-API error message.
pub const FILE_API_ERROR_BODY_LIMIT: usize = 256;

/// A directory/file entry returned by the sandbox file-API list endpoint.
///
/// Mirrors the Java `client/model/FileEntry` JSON shape (`name` / `size` / `type` /
/// `mod_time`); unknown fields are ignored and missing fields default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Entry name (without the parent directory).
    #[serde(default)]
    pub name: String,
    /// Entry size in bytes.
    #[serde(default)]
    pub size: i64,
    /// Entry type: either `file` or `directory`.
    #[serde(default, rename = "type")]
    pub entry_type: String,
    /// Last modification time as a POSIX timestamp.
    #[serde(default, rename = "mod_time")]
    pub mod_time: f64,
}

impl FileEntry {
    /// Returns whether this entry is a directory (Java `type == "directory"`).
    pub fn is_directory(&self) -> bool {
        self.entry_type.eq_ignore_ascii_case("directory")
            || self.entry_type.eq_ignore_ascii_case("dir")
    }

    /// Returns whether this entry is a regular file (Java `type == "file"`).
    pub fn is_file(&self) -> bool {
        self.entry_type.eq_ignore_ascii_case("file")
    }
}

/// Parses the list endpoint body. Blank / `null` bodies yield an empty list (mirrors Java).
pub fn parse_file_entries(body: &str) -> SandboxResult<Vec<FileEntry>> {
    let trimmed = body.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(Vec::new());
    }
    serde_json::from_str::<Vec<FileEntry>>(trimmed).map_err(|e| {
        SandboxExtensionError::Serialization(format!("invalid file-API list payload: {e}"))
    })
}

/// Parses the exists endpoint body (`{"exists":true}`). Missing fields mean `false`.
pub fn parse_exists_result(body: &str) -> SandboxResult<bool> {
    #[derive(Deserialize)]
    struct ExistsResult {
        #[serde(default)]
        exists: bool,
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Ok(false);
    }
    let parsed: ExistsResult = serde_json::from_str(trimmed).map_err(|e| {
        SandboxExtensionError::Serialization(format!("invalid file-API exists payload: {e}"))
    })?;
    Ok(parsed.exists)
}

/// Percent-encodes a path for interpolation into a file-API URL.
///
/// Mirrors Java `URLEncoder.encode(...)` with `+` replaced by `%20`; unreserved characters
/// (`A-Z a-z 0-9 - _ . ~`) are emitted verbatim.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let c = *byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Normalizes a POSIX path (simplified `posixpath.normpath`), mirroring `Filesystem.normalizePosix`.
fn normalize_posix(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                if stack.last().map(|last| *last != "..").unwrap_or(false) {
                    stack.pop();
                } else if !absolute {
                    stack.push("..");
                }
            }
            other => stack.push(other),
        }
    }
    if stack.is_empty() {
        return if absolute {
            "/".to_string()
        } else {
            ".".to_string()
        };
    }
    let joined = stack.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// Returns a relative, `..`-free filename safe to send as a multipart filename.
///
/// Mirrors Java `Filesystem.safeUploadPath`: rejects empty paths, ASCII control characters,
/// paths that normalize to nothing, and paths escaping the sandbox root.
pub fn safe_upload_path(path: &str) -> SandboxResult<String> {
    if path.is_empty() {
        return Err(SandboxExtensionError::Configuration(
            "upload path cannot be empty".to_string(),
        ));
    }
    if path.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7F) {
        return Err(SandboxExtensionError::Configuration(format!(
            "upload path contains ASCII control characters: {path}"
        )));
    }
    let stripped = path.trim();
    if stripped.is_empty() {
        return Err(SandboxExtensionError::Configuration(
            "upload path cannot be empty".to_string(),
        ));
    }
    let mut normalized = normalize_posix(stripped);
    while let Some(rest) = normalized.strip_prefix('/') {
        normalized = rest.to_string();
    }
    if normalized.is_empty() || normalized == "." {
        return Err(SandboxExtensionError::Configuration(format!(
            "upload path '{path}' does not name a file"
        )));
    }
    if normalized.split('/').any(|part| part == "..") {
        return Err(SandboxExtensionError::Configuration(format!(
            "upload path '{path}' escapes the sandbox root"
        )));
    }
    Ok(normalized)
}

/// Builds a `multipart/form-data` body carrying one file part (mirrors `Filesystem.buildMultipartBody`).
pub fn build_multipart_body(filename: &str, content: &[u8], boundary: &str) -> Vec<u8> {
    let prefix = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    );
    let suffix = format!("\r\n--{boundary}--\r\n");
    let mut body = Vec::with_capacity(prefix.len() + content.len() + suffix.len());
    body.extend_from_slice(prefix.as_bytes());
    body.extend_from_slice(content);
    body.extend_from_slice(suffix.as_bytes());
    body
}

/// Truncates a body for inclusion in an error message.
pub fn truncate(value: &str, max_len: usize) -> String {
    if value.len() > max_len {
        format!("{}...", &value[..max_len])
    } else {
        value.to_string()
    }
}

/// A raw response from the sandbox file API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileApiResponse {
    /// HTTP status code.
    pub status: u16,
    /// Raw response body.
    pub body: Vec<u8>,
}

impl FileApiResponse {
    /// Returns whether the status code is 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Interprets the body as UTF-8 without failing on invalid sequences.
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Transport seam for the sandbox file API (implemented by [`ReqwestFileApiTransport`]).
#[async_trait]
pub trait FileApiTransport: Send + Sync + Debug {
    /// Performs a `GET`.
    async fn get(
        &self,
        url: String,
        headers: Vec<(String, String)>,
    ) -> SandboxResult<FileApiResponse>;

    /// Performs a `POST` with a raw body and `Content-Type`.
    async fn post_bytes(
        &self,
        url: String,
        content_type: String,
        body: Vec<u8>,
        headers: Vec<(String, String)>,
    ) -> SandboxResult<FileApiResponse>;
}

/// Seam for the sandbox command channel, used by operations the file API cannot express (move).
#[async_trait]
pub trait ExecTransport: Send + Sync + Debug {
    /// Runs `command` and returns its captured output.
    async fn exec(&self, command: &str, timeout_seconds: u64) -> SandboxResult<RemoteExecOutput>;
}

/// `reqwest`-backed [`FileApiTransport`].
#[derive(Debug, Clone)]
pub struct ReqwestFileApiTransport {
    client: Client,
}

impl ReqwestFileApiTransport {
    /// Wraps a `reqwest` client.
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl FileApiTransport for ReqwestFileApiTransport {
    async fn get(
        &self,
        url: String,
        headers: Vec<(String, String)>,
    ) -> SandboxResult<FileApiResponse> {
        let mut request = self.client.get(&url);
        for (key, value) in headers {
            request = request.header(key, value);
        }
        to_response(request.send().await).await
    }

    async fn post_bytes(
        &self,
        url: String,
        content_type: String,
        body: Vec<u8>,
        headers: Vec<(String, String)>,
    ) -> SandboxResult<FileApiResponse> {
        let mut request = self
            .client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, content_type);
        for (key, value) in headers {
            request = request.header(key, value);
        }
        to_response(request.body(body).send().await).await
    }
}

async fn to_response(
    response: Result<reqwest::Response, reqwest::Error>,
) -> SandboxResult<FileApiResponse> {
    let response = response.map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
    let status = response.status().as_u16();
    let body = response
        .bytes()
        .await
        .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?
        .to_vec();
    Ok(FileApiResponse { status, body })
}

/// Sandbox file API client (`client/Filesystem`).
#[derive(Debug, Clone)]
pub struct KubernetesFileApi {
    base_url: String,
    transport: Arc<dyn FileApiTransport>,
    exec: Option<Arc<dyn ExecTransport>>,
    headers: Vec<(String, String)>,
}

impl KubernetesFileApi {
    /// Creates a file-API client rooted at `base_url`.
    pub fn new(base_url: impl Into<String>, transport: Arc<dyn FileApiTransport>) -> Self {
        Self {
            base_url: base_url.into(),
            transport,
            exec: None,
            headers: Vec::new(),
        }
    }

    /// Attaches default headers (e.g. router `X-Sandbox-*` headers).
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }

    /// Attaches a command transport so [`move_path`](Self::move_path) is usable.
    pub fn with_exec(mut self, exec: Arc<dyn ExecTransport>) -> Self {
        self.exec = Some(exec);
        self
    }

    /// Returns the configured base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Returns the default headers.
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    fn url(&self, path: &str) -> String {
        join_url(&self.base_url, path)
    }

    /// Uploads `content` to `path` (relative to the file-API base dir).
    pub async fn write(&self, path: &str, content: &[u8]) -> SandboxResult<()> {
        let safe = safe_upload_path(path)?;
        let boundary = format!("----AgentScopeSandbox{}", uuid::Uuid::new_v4().simple());
        let body = build_multipart_body(&safe, content, &boundary);
        let response = self
            .transport
            .post_bytes(
                self.url(FILE_API_UPLOAD_PATH),
                format!("multipart/form-data; boundary={boundary}"),
                body,
                self.headers.clone(),
            )
            .await?;
        if !response.is_success() {
            return Err(SandboxExtensionError::http(
                response.status,
                format!(
                    "upload failed: {}",
                    truncate(&response.body_text(), FILE_API_ERROR_BODY_LIMIT)
                ),
            ));
        }
        Ok(())
    }

    /// Uploads UTF-8 text to `path`.
    pub async fn write_text(&self, path: &str, content: &str) -> SandboxResult<()> {
        self.write(path, content.as_bytes()).await
    }

    /// Downloads the file at `path`.
    pub async fn read(&self, path: &str) -> SandboxResult<Vec<u8>> {
        let safe = safe_upload_path(path)?;
        let url = self.url(&format!(
            "{FILE_API_DOWNLOAD_PREFIX}/{}",
            percent_encode(&safe)
        ));
        let response = self.transport.get(url, self.headers.clone()).await?;
        if !response.is_success() {
            return Err(SandboxExtensionError::http(
                response.status,
                format!("download failed for path: {path}"),
            ));
        }
        Ok(response.body)
    }

    /// Lists the directory at `path`.
    pub async fn list(&self, path: &str) -> SandboxResult<Vec<FileEntry>> {
        let url = self.url(&format!("{FILE_API_LIST_PREFIX}/{}", percent_encode(path)));
        let response = self.transport.get(url, self.headers.clone()).await?;
        if !response.is_success() {
            return Err(SandboxExtensionError::http(
                response.status,
                format!("list failed for path: {path}"),
            ));
        }
        parse_file_entries(&response.body_text())
    }

    /// Checks whether `path` exists.
    pub async fn exists(&self, path: &str) -> SandboxResult<bool> {
        let url = self.url(&format!(
            "{FILE_API_EXISTS_PREFIX}/{}",
            percent_encode(path)
        ));
        let response = self.transport.get(url, self.headers.clone()).await?;
        if !response.is_success() {
            return Err(SandboxExtensionError::http(
                response.status,
                format!("exists check failed for path: {path}"),
            ));
        }
        parse_exists_result(&response.body_text())
    }

    /// Moves a file inside the sandbox (expressed as an exec, since the file API has no move).
    pub async fn move_path(&self, from: &str, to: &str) -> SandboxResult<()> {
        let exec = self.exec.as_ref().ok_or_else(|| {
            SandboxExtensionError::Configuration(
                "move requires an exec transport (KubernetesFileApi::with_exec)".to_string(),
            )
        })?;
        let command = build_move_command(from, to);
        let output = exec.exec(&command, 60).await?;
        if output.exit_code != 0 {
            return Err(SandboxExtensionError::Runtime(
                crate::error::SandboxErrorCode::CommandExecutionError,
                format!("move failed (exit {}): {}", output.exit_code, output.stderr),
            ));
        }
        Ok(())
    }
}

/// Builds the shell command implementing a move (creating the destination parent first).
pub fn build_move_command(from: &str, to: &str) -> String {
    match crate::workspace::parent_dir(to) {
        Some(parent) => format!(
            "mkdir -p {} && mv {} {}",
            crate::workspace::shell_quote(&parent),
            crate::workspace::shell_quote(from),
            crate::workspace::shell_quote(to)
        ),
        None => format!(
            "mv {} {}",
            crate::workspace::shell_quote(from),
            crate::workspace::shell_quote(to)
        ),
    }
}

/// Builds the absolute sandbox path of `relative` under the file-API base dir.
///
/// Mirrors the Java `KubernetesSandbox#fileApiPath`: a trailing slash on the base dir is
/// tolerated and exactly one separator is emitted.
pub fn file_api_path(base_dir: &str, relative: &str) -> String {
    let base = base_dir.trim_end_matches('/');
    format!("{base}/{relative}")
}

/// Builds a unique workspace-archive path relative to the file-API base dir.
///
/// Mirrors the Java `KubernetesSandbox#tempArchiveName`: a per-session hash prefix plus a UUID
/// keeps concurrent persist/hydrate calls from sharing (and deleting) each other's archive.
pub fn temp_archive_rel_path(prefix: &str, session_id: &str) -> String {
    format!(
        "{FILE_API_TMP_DIR_REL}/{prefix}-{:x}-{}.tar",
        crate::workspace::short_hash(session_id),
        uuid::Uuid::new_v4()
    )
}

/// Maps an absolute sandbox path to a file-API-relative path, or `None` when untransferable.
///
/// Mirrors `KubernetesSandbox#toFileApiRelative`: the file API is disabled when the base dir is
/// blank; paths outside the base dir, the base dir itself and paths containing empty / `.` /
/// `..` segments fall back to the exec transfer strategy.
pub fn to_file_api_relative(base_dir: &str, absolute_path: &str) -> Option<String> {
    if base_dir.trim().is_empty() || absolute_path.is_empty() {
        return None;
    }
    let norm_base = base_dir.trim_end_matches('/');
    let remainder = absolute_path.strip_prefix(&format!("{norm_base}/"))?;
    if remainder.is_empty() {
        return None;
    }
    for part in remainder.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
    }
    Some(remainder.to_string())
}

/// Single-file transfer contract declared locally (Rust harness has no `SandboxFileTransfer`).
#[async_trait]
pub trait SandboxFileTransfer: Send + Sync + Debug {
    /// Returns whether `absolute_path` can be transferred through the runtime file API.
    fn supports_file_transfer(&self, absolute_path: &str) -> bool;

    /// Uploads `content` to `absolute_path` (used by the harness filesystem on write).
    async fn upload_file(&self, absolute_path: &str, content: &[u8]) -> SandboxResult<()>;

    /// Downloads the file at `absolute_path`.
    async fn download_file(&self, absolute_path: &str) -> SandboxResult<Vec<u8>>;

    /// Marks the underlying connection as closed (called when the sandbox is released).
    ///
    /// Mirrors the Java `KubernetesSandbox#requireActiveConnection` (commit `19fdc9e6d`): a
    /// transfer adapter that outlives its sandbox must fail with
    /// [`SandboxErrorCode::ConnectionClosed`] instead of sending requests to a dead router.
    /// Default: no-op, for backends that do not model a connection lifecycle.
    fn close(&self) {}
}

/// `SandboxFileTransfer` backed by the runtime file API and an optional exec fallback.
#[derive(Debug, Clone)]
pub struct KubernetesFileTransfer {
    api: KubernetesFileApi,
    file_api_base_dir: String,
    closed: Arc<AtomicBool>,
}

impl KubernetesFileTransfer {
    /// Creates a transfer adapter for the given file API and base dir.
    pub fn new(api: KubernetesFileApi, file_api_base_dir: impl Into<String>) -> Self {
        Self {
            api,
            file_api_base_dir: file_api_base_dir.into(),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns whether the sandbox connection was closed.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Rejects transfers once the sandbox was released.
    fn ensure_active(&self) -> SandboxResult<()> {
        if self.is_closed() {
            return Err(SandboxExtensionError::Runtime(
                SandboxErrorCode::ConnectionClosed,
                "Kubernetes sandbox connection has been closed".to_string(),
            ));
        }
        Ok(())
    }

    /// Returns the configured file-API base dir.
    pub fn file_api_base_dir(&self) -> &str {
        &self.file_api_base_dir
    }

    /// Returns the wrapped file API.
    pub fn api(&self) -> &KubernetesFileApi {
        &self.api
    }

    /// Resolves the relative path or fails with the Java equivalent of `requireFileApiRelative`.
    fn require_relative(&self, absolute_path: &str) -> SandboxResult<String> {
        to_file_api_relative(&self.file_api_base_dir, absolute_path).ok_or_else(|| {
            SandboxExtensionError::Configuration(format!(
                "path not transferable via the runtime file API: {absolute_path}"
            ))
        })
    }
}

#[async_trait]
impl SandboxFileTransfer for KubernetesFileTransfer {
    fn supports_file_transfer(&self, absolute_path: &str) -> bool {
        to_file_api_relative(&self.file_api_base_dir, absolute_path).is_some()
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    async fn upload_file(&self, absolute_path: &str, content: &[u8]) -> SandboxResult<()> {
        self.ensure_active()?;
        let relative = self.require_relative(absolute_path)?;
        // The runtime file API is not required to create parent directories.
        if let Some(parent) = crate::workspace::parent_dir(absolute_path) {
            if let Some(exec) = self.api.exec.as_ref() {
                let output = exec
                    .exec(
                        &format!("mkdir -p {}", crate::workspace::shell_quote(&parent)),
                        60,
                    )
                    .await?;
                if output.exit_code != 0 {
                    return Err(SandboxExtensionError::Runtime(
                        crate::error::SandboxErrorCode::WorkspaceStartError,
                        format!("mkdir failed for {absolute_path}: {}", output.stderr),
                    ));
                }
            }
        }
        self.api.write(&relative, content).await
    }

    async fn download_file(&self, absolute_path: &str) -> SandboxResult<Vec<u8>> {
        self.ensure_active()?;
        let relative = self.require_relative(absolute_path)?;
        self.api.read(&relative).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn safe_upload_path_normalizes_relative() {
        assert_eq!(safe_upload_path("foo/bar.txt").unwrap(), "foo/bar.txt");
        assert_eq!(safe_upload_path("/foo/bar.txt").unwrap(), "foo/bar.txt");
        assert_eq!(safe_upload_path("./foo/bar.txt").unwrap(), "foo/bar.txt");
        assert_eq!(safe_upload_path("foo/bar/../baz").unwrap(), "foo/baz");
    }

    #[test]
    fn safe_upload_path_rejects_escape() {
        assert!(safe_upload_path("../etc/passwd").is_err());
        assert!(safe_upload_path("").is_err());
        assert!(safe_upload_path(".").is_err());
        assert!(safe_upload_path("foo\u{0}../etc/passwd").is_err());
    }

    #[test]
    fn percent_encode_matches_urlencoder() {
        assert_eq!(percent_encode("workspace/file.txt"), "workspace%2Ffile.txt");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("keep-_.~ok"), "keep-_.~ok");
    }

    #[test]
    fn multipart_body_has_boundaries_and_content() {
        let body = build_multipart_body("foo.txt", b"hi", "BOUND");
        let text = String::from_utf8(body.clone()).unwrap();
        assert!(text.starts_with("--BOUND\r\n"));
        assert!(text.contains("filename=\"foo.txt\""));
        assert!(text.contains("\r\n\r\nhi\r\n"));
        assert!(text.ends_with("--BOUND--\r\n"));
    }

    #[test]
    fn file_entry_parses_mod_time_and_type() {
        let raw = r#"[{"name":"a.txt","size":12,"type":"file","mod_time":123.5},{"name":"d","size":0,"type":"directory"}]"#;
        let entries = parse_file_entries(raw).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_file());
        assert_eq!(entries[0].size, 12);
        assert!((entries[0].mod_time - 123.5).abs() < f64::EPSILON);
        assert!(entries[1].is_directory());
    }

    #[test]
    fn file_entry_tolerates_unknown_and_missing_fields() {
        let entries = parse_file_entries(r#"[{"name":"x","extra":1}]"#).unwrap();
        assert_eq!(entries[0].name, "x");
        assert_eq!(entries[0].size, 0);
        assert!(!entries[0].is_file());
    }

    #[test]
    fn blank_list_body_is_empty() {
        assert!(parse_file_entries("").unwrap().is_empty());
        assert!(parse_file_entries("  null ").unwrap().is_empty());
        assert!(parse_file_entries("[]").unwrap().is_empty());
    }

    #[test]
    fn exists_body_parsing() {
        assert!(parse_exists_result(r#"{"exists":true}"#).unwrap());
        assert!(!parse_exists_result(r#"{"exists":false}"#).unwrap());
        assert!(!parse_exists_result("").unwrap());
    }

    #[test]
    fn to_file_api_relative_rules() {
        assert_eq!(
            to_file_api_relative("/workspace", "/workspace/a/b.txt").as_deref(),
            Some("a/b.txt")
        );
        assert_eq!(
            to_file_api_relative("/workspace/", "/workspace/a").as_deref(),
            Some("a")
        );
        assert_eq!(to_file_api_relative("", "/workspace/a"), None);
        assert_eq!(to_file_api_relative("/workspace", "/etc/hosts"), None);
        assert_eq!(to_file_api_relative("/workspace", "/workspace"), None);
        assert_eq!(
            to_file_api_relative("/workspace", "/workspace/a/../b"),
            None
        );
        assert_eq!(to_file_api_relative("/workspace", "/workspace/a//b"), None);
    }

    #[test]
    fn file_api_path_and_temp_archive_names_are_deterministic() {
        assert_eq!(
            file_api_path("/workspace", ".agentscope-tmp/x.tar"),
            "/workspace/.agentscope-tmp/x.tar"
        );
        assert_eq!(
            file_api_path("/workspace/", ".agentscope-tmp/x.tar"),
            "/workspace/.agentscope-tmp/x.tar"
        );
        let first = temp_archive_rel_path("ws-persist", "sess-1");
        let second = temp_archive_rel_path("ws-persist", "sess-1");
        assert!(first.starts_with(&format!("{FILE_API_TMP_DIR_REL}/ws-persist-")));
        assert!(first.ends_with(".tar"));
        // Concurrent transfers must never share an archive name (Java `tempArchiveName`).
        assert_ne!(first, second);
        assert!(to_file_api_relative("/workspace", &file_api_path("/workspace", &first)).is_some());
    }

    #[test]
    fn move_command_creates_parent() {
        let command = build_move_command("/workspace/a.txt", "/workspace/sub/b.txt");
        assert!(command.contains("mkdir -p /workspace/sub"));
        assert!(command.contains("mv /workspace/a.txt /workspace/sub/b.txt"));
    }

    /// Recording transport used to exercise the file API offline.
    #[derive(Debug, Default)]
    struct FakeTransport {
        gets: Mutex<Vec<String>>,
        posts: Mutex<Vec<(String, String, Vec<u8>)>>,
    }

    #[async_trait]
    impl FileApiTransport for FakeTransport {
        async fn get(
            &self,
            url: String,
            _headers: Vec<(String, String)>,
        ) -> SandboxResult<FileApiResponse> {
            self.gets.lock().unwrap().push(url.clone());
            if url.ends_with("list/workspace") {
                Ok(FileApiResponse {
                    status: 200,
                    body: br#"[{"name":"a.txt","size":1,"type":"file"}]"#.to_vec(),
                })
            } else if url.contains("download/") {
                Ok(FileApiResponse {
                    status: 200,
                    body: b"file content".to_vec(),
                })
            } else if url.contains("exists/") {
                Ok(FileApiResponse {
                    status: 200,
                    body: br#"{"exists":true}"#.to_vec(),
                })
            } else {
                Ok(FileApiResponse {
                    status: 404,
                    body: b"missing".to_vec(),
                })
            }
        }

        async fn post_bytes(
            &self,
            url: String,
            content_type: String,
            body: Vec<u8>,
            _headers: Vec<(String, String)>,
        ) -> SandboxResult<FileApiResponse> {
            self.posts.lock().unwrap().push((url, content_type, body));
            Ok(FileApiResponse {
                status: 200,
                body: Vec::new(),
            })
        }
    }

    #[derive(Debug, Default)]
    struct FakeExec {
        commands: Mutex<Vec<String>>,
        exit_code: i32,
    }

    #[async_trait]
    impl ExecTransport for FakeExec {
        async fn exec(&self, command: &str, _timeout: u64) -> SandboxResult<RemoteExecOutput> {
            self.commands.lock().unwrap().push(command.to_string());
            Ok(RemoteExecOutput::new(
                self.exit_code,
                String::new(),
                String::new(),
            ))
        }
    }

    fn api(transport: Arc<FakeTransport>) -> KubernetesFileApi {
        KubernetesFileApi::new("http://127.0.0.1:8888", transport)
            .with_headers(vec![("X-Sandbox-ID".to_string(), "sbx-1".to_string())])
    }

    #[tokio::test]
    async fn write_posts_multipart_to_upload() {
        let transport = Arc::new(FakeTransport::default());
        api(transport.clone())
            .write_text("foo.txt", "hi")
            .await
            .unwrap();
        let posts = transport.posts.lock().unwrap().clone();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].0, "http://127.0.0.1:8888/upload");
        assert!(posts[0].1.starts_with("multipart/form-data; boundary="));
        let body = String::from_utf8(posts[0].2.clone()).unwrap();
        assert!(body.contains("filename=\"foo.txt\""));
        assert!(body.contains("hi"));
    }

    #[tokio::test]
    async fn read_downloads_percent_encoded() {
        let transport = Arc::new(FakeTransport::default());
        let body = api(transport.clone())
            .read("workspace/file.txt")
            .await
            .unwrap();
        assert_eq!(body, b"file content");
        let gets = transport.gets.lock().unwrap().clone();
        assert_eq!(
            gets[0],
            "http://127.0.0.1:8888/download/workspace%2Ffile.txt"
        );
    }

    #[tokio::test]
    async fn list_parses_entries() {
        let transport = Arc::new(FakeTransport::default());
        let entries = api(transport.clone()).list("workspace").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a.txt");
        assert_eq!(
            transport.gets.lock().unwrap()[0],
            "http://127.0.0.1:8888/list/workspace"
        );
    }

    #[tokio::test]
    async fn exists_returns_true() {
        let transport = Arc::new(FakeTransport::default());
        assert!(api(transport.clone())
            .exists("workspace/file.txt")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn non_success_status_is_http_error() {
        #[derive(Debug)]
        struct ErrTransport;
        #[async_trait]
        impl FileApiTransport for ErrTransport {
            async fn get(
                &self,
                _u: String,
                _h: Vec<(String, String)>,
            ) -> SandboxResult<FileApiResponse> {
                Ok(FileApiResponse {
                    status: 500,
                    body: b"boom".to_vec(),
                })
            }
            async fn post_bytes(
                &self,
                _u: String,
                _c: String,
                _b: Vec<u8>,
                _h: Vec<(String, String)>,
            ) -> SandboxResult<FileApiResponse> {
                Ok(FileApiResponse {
                    status: 500,
                    body: b"boom".to_vec(),
                })
            }
        }
        let api = KubernetesFileApi::new("http://x", Arc::new(ErrTransport));
        let err = api.list("workspace").await.unwrap_err();
        assert_eq!(err.status(), Some(500));
        assert!(api.write_text("a", "b").await.is_err());
    }

    #[tokio::test]
    async fn move_requires_exec_transport() {
        let transport = Arc::new(FakeTransport::default());
        let err = api(transport).move_path("a", "b").await.unwrap_err();
        assert!(matches!(err, SandboxExtensionError::Configuration(_)));
    }

    #[tokio::test]
    async fn move_runs_command_through_exec() {
        let transport = Arc::new(FakeTransport::default());
        let exec = Arc::new(FakeExec::default());
        let api = api(transport).with_exec(exec.clone());
        api.move_path("/workspace/a.txt", "/workspace/b.txt")
            .await
            .unwrap();
        let commands = exec.commands.lock().unwrap().clone();
        assert_eq!(commands.len(), 1);
        assert!(commands[0].contains("mv /workspace/a.txt /workspace/b.txt"));
    }

    #[tokio::test]
    async fn file_transfer_gates_on_base_dir() {
        let transfer =
            KubernetesFileTransfer::new(api(Arc::new(FakeTransport::default())), "/workspace");
        assert!(transfer.supports_file_transfer("/workspace/a.txt"));
        assert!(!transfer.supports_file_transfer("/etc/hosts"));
        let bytes = transfer.download_file("/workspace/a.txt").await.unwrap();
        assert_eq!(bytes, b"file content");
        assert!(transfer.download_file("/etc/hosts").await.is_err());
    }

    #[tokio::test]
    async fn file_transfer_upload_creates_parent_and_uploads() {
        let transport = Arc::new(FakeTransport::default());
        let exec = Arc::new(FakeExec::default());
        let file_api = api(transport.clone()).with_exec(exec.clone());
        let transfer = KubernetesFileTransfer::new(file_api, "/workspace");
        transfer
            .upload_file("/workspace/sub/a.txt", b"data")
            .await
            .unwrap();
        let commands = exec.commands.lock().unwrap().clone();
        assert_eq!(commands, vec!["mkdir -p /workspace/sub".to_string()]);
        let posts = transport.posts.lock().unwrap().clone();
        assert_eq!(posts.len(), 1);
        assert!(String::from_utf8_lossy(&posts[0].2).contains("filename=\"sub/a.txt\""));
    }

    #[tokio::test]
    async fn closed_transfer_reports_connection_closed_without_a_request() {
        // Java `KubernetesSandbox#requireActiveConnection`（commit 19fdc9e6d）：沙箱释放后
        // 继续使用旧的传输适配器必须报 SANDBOX_CONNECTION_CLOSED，而不是打到已死的 router。
        let transport = Arc::new(FakeTransport::default());
        let exec = Arc::new(FakeExec::default());
        let transfer = KubernetesFileTransfer::new(
            api(transport.clone()).with_exec(exec.clone()),
            "/workspace",
        );
        assert!(!transfer.is_closed());
        transfer.close();
        assert!(transfer.is_closed());
        // `supports_file_transfer` 与 Java 一致，仍只看路径前缀。
        assert!(transfer.supports_file_transfer("/workspace/a.txt"));

        for err in [
            transfer
                .upload_file("/workspace/a.txt", b"x")
                .await
                .unwrap_err(),
            transfer
                .download_file("/workspace/a.txt")
                .await
                .unwrap_err(),
        ] {
            match err {
                SandboxExtensionError::Runtime(SandboxErrorCode::ConnectionClosed, message) => {
                    assert_eq!(message, "Kubernetes sandbox connection has been closed");
                }
                other => panic!("expected a closed-connection error, got {other:?}"),
            }
        }
        assert!(transport.gets.lock().unwrap().is_empty());
        assert!(transport.posts.lock().unwrap().is_empty());
        assert!(exec.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn truncate_appends_ellipsis() {
        assert_eq!(truncate("abcdef", 3), "abc...");
        assert_eq!(truncate("abc", 6), "abc");
    }
}
