//! Shared skeleton for the remote (cloud) sandbox backends.
//!
//! Java models each backend as a concrete `AbstractBaseSandbox` subclass plus its own
//! `SandboxClient`/`SandboxState` subclass. Rust's harness contract is narrower: [`Sandbox`]
//! and [`SandboxClient`] are traits, and `SandboxState` is a **closed enum**
//! (`Docker`/`Local`/`Remote`). We therefore adapt every backend onto the harness via a
//! backend-agnostic [`RemoteBackend`] and a [`RemoteSandboxStateData`] carrier that projects
//! into [`SandboxState::Remote`] (backend-specific fields live in `metadata`).
//!
//! The shared pieces are: lifecycle orchestration ([`RemoteSandbox`]), the harness
//! [`SandboxClient`] implementation ([`RemoteSandboxClient`]), tar/base64 workspace
//! persistence executed through the backend's exec channel, and pure helpers that are unit
//! tested without any network access.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::sandbox::{
    ExecResult, RemoteSandboxState, Sandbox, SandboxClient, SandboxClientOptions, SandboxSnapshot,
    SandboxSnapshotRef, SandboxSnapshotSpec, SandboxState, WorkspaceSpec,
};

use crate::error::{SandboxErrorCode, SandboxExtensionError, SandboxResult};
use crate::kubernetes_files::SandboxFileTransfer;
use crate::workspace::{build_workspace_setup_script, shell_quote, short_hash};

/// Metadata key holding the backend discriminator (`e2b`, `daytona`, `agentrun`, `kubernetes`).
pub const BACKEND_METADATA_KEY: &str = "agentscope.sandbox.backend";
/// Metadata key holding the remote workspace root.
pub const WORKSPACE_ROOT_METADATA_KEY: &str = "agentscope.sandbox.workspace_root";
/// Metadata key recording whether the SDK owns the remote sandbox lifecycle.
pub const OWNED_METADATA_KEY: &str = "agentscope.sandbox.sandbox_owned";
/// Metadata key holding the JSON-serialized backend-specific extras.
pub const EXTRA_METADATA_KEY: &str = "agentscope.sandbox.extra";
/// Metadata key holding the JSON-serialized [`WorkspaceSpec`].
pub const WORKSPACE_SPEC_METADATA_KEY: &str = "agentscope.sandbox.workspace_spec";

/// Remote path used to stage base64 workspace archives during hydration.
pub const WS_B64_TMP_PATH: &str = "/tmp/agentscope-ws.b64";
/// Number of base64 characters per hydration chunk (mirrors the Java `B64_CHUNK`).
pub const B64_CHUNK: usize = 4000;
/// Default timeout for tar archive operations, in seconds (mirrors Java `TAR_TIMEOUT_SECONDS`).
pub const TAR_TIMEOUT_SECONDS: u64 = 300;

/// Output of a command executed through a backend's exec channel.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteExecOutput {
    /// Process exit code.
    pub exit_code: i32,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Whether the output was truncated by the backend.
    pub truncated: bool,
}

impl RemoteExecOutput {
    /// Creates an output with the given exit code and captured streams.
    pub fn new(exit_code: i32, stdout: String, stderr: String) -> Self {
        Self {
            exit_code,
            stdout,
            stderr,
            truncated: false,
        }
    }

    /// Projects onto the harness [`ExecResult`].
    pub fn to_exec_result(&self) -> ExecResult {
        ExecResult {
            stdout: self.stdout.clone(),
            stderr: self.stderr.clone(),
            exit_code: self.exit_code,
            truncated: self.truncated,
        }
    }
}

/// Opaque handle to a live remote sandbox (id + backend-specific fields).
#[derive(Debug, Clone)]
pub struct RemoteSandboxHandle {
    /// Backend sandbox id.
    pub id: String,
    /// Backend-specific fields learned at create/connect time (domain, tokens, ...).
    pub extra: BTreeMap<String, String>,
}

impl RemoteSandboxHandle {
    /// Creates a handle from an id and backend-specific extras.
    pub fn new(id: impl Into<String>, extra: BTreeMap<String, String>) -> Self {
        Self {
            id: id.into(),
            extra,
        }
    }
}

/// Backend-agnostic, serializable sandbox state.
///
/// Projected into the harness [`SandboxState::Remote`] variant: the harness enum is closed, so
/// backend-specific fields are carried inside `RemoteSandboxState::metadata`.
#[derive(Debug, Clone)]
pub struct RemoteSandboxStateData {
    /// Session id assigned at create time.
    pub session_id: String,
    /// Backend discriminator.
    pub backend: String,
    /// Remote sandbox id (absent until created).
    pub sandbox_id: Option<String>,
    /// Human-readable endpoint / base URL (informational, mirrors `RemoteSandboxState::endpoint`).
    pub endpoint: String,
    /// Access token (mirrors `RemoteSandboxState::token`).
    pub token: Option<String>,
    /// Absolute workspace root inside the remote sandbox.
    pub workspace_root: String,
    /// Whether the SDK owns the sandbox lifecycle (create/stop/delete).
    pub sandbox_owned: bool,
    /// Whether the workspace has been fully initialized at the last stop.
    pub workspace_root_ready: bool,
    /// Hash of the last applied workspace projection.
    pub workspace_projection_hash: Option<String>,
    /// Backend-specific scalars.
    pub extra: BTreeMap<String, String>,
    /// Workspace specification used to (re)build the remote workspace.
    pub workspace_spec: Option<WorkspaceSpec>,
    /// Snapshot the workspace was persisted as (identity only — the client is re-bound by the
    /// snapshot spec on resume, mirroring Java `RemoteSandboxSnapshot`'s `@JsonIgnore` client).
    pub snapshot: Option<SandboxSnapshotRef>,
}

impl RemoteSandboxStateData {
    /// Projects this state into the harness [`SandboxState::Remote`] variant.
    ///
    /// Serialization failures for the optional `extra`/`workspace_spec` blobs are logged and
    /// skipped rather than failing the projection.
    pub fn to_harness_state(&self) -> SandboxState {
        let mut metadata: HashMap<String, String> = HashMap::new();
        metadata.insert(BACKEND_METADATA_KEY.to_string(), self.backend.clone());
        metadata.insert(
            WORKSPACE_ROOT_METADATA_KEY.to_string(),
            self.workspace_root.clone(),
        );
        metadata.insert(
            OWNED_METADATA_KEY.to_string(),
            self.sandbox_owned.to_string(),
        );
        if !self.extra.is_empty() {
            match serde_json::to_string(&self.extra) {
                Ok(json) => {
                    metadata.insert(EXTRA_METADATA_KEY.to_string(), json);
                }
                Err(e) => tracing::warn!("sandbox: failed to serialize extra state: {e}"),
            }
        }
        if let Some(spec) = &self.workspace_spec {
            match serde_json::to_string(spec) {
                Ok(json) => {
                    metadata.insert(WORKSPACE_SPEC_METADATA_KEY.to_string(), json);
                }
                Err(e) => tracing::warn!("sandbox: failed to serialize workspace spec: {e}"),
            }
        }
        // `RemoteSandboxState` is now exported by the harness, so the projection is a direct
        // struct construction (it used to go through a JSON round-trip and could not carry the
        // snapshot identity).
        SandboxState::Remote(RemoteSandboxState {
            session_id: self.session_id.clone(),
            endpoint: self.endpoint.clone(),
            token: self.token.clone(),
            workspace_root_ready: self.workspace_root_ready,
            workspace_projection_hash: self.workspace_projection_hash.clone(),
            metadata,
            snapshot: self.snapshot.clone(),
        })
    }

    /// Reconstructs typed state from a harness [`SandboxState`].
    ///
    /// Only the [`SandboxState::Remote`] variant is accepted; other variants are rejected so a
    /// backend never silently resumes a container/local sandbox.
    pub fn from_harness_state(state: &SandboxState) -> SandboxResult<Self> {
        let remote = match state {
            SandboxState::Remote(remote) => remote,
            other => {
                return Err(SandboxExtensionError::Configuration(format!(
                    "expected a Remote sandbox state, got {other:?}"
                )))
            }
        };
        let backend = remote
            .metadata
            .get(BACKEND_METADATA_KEY)
            .cloned()
            .unwrap_or_default();
        let workspace_root = remote
            .metadata
            .get(WORKSPACE_ROOT_METADATA_KEY)
            .cloned()
            .unwrap_or_else(|| "/workspace".to_string());
        let sandbox_owned = remote
            .metadata
            .get(OWNED_METADATA_KEY)
            .map(|v| v != "false")
            .unwrap_or(true);
        let extra = remote
            .metadata
            .get(EXTRA_METADATA_KEY)
            .and_then(|raw| serde_json::from_str::<BTreeMap<String, String>>(raw).ok())
            .unwrap_or_default();
        let workspace_spec = remote
            .metadata
            .get(WORKSPACE_SPEC_METADATA_KEY)
            .and_then(|raw| serde_json::from_str::<WorkspaceSpec>(raw).ok());
        Ok(Self {
            session_id: remote.session_id.clone(),
            backend,
            sandbox_id: None,
            endpoint: remote.endpoint.clone(),
            token: remote.token.clone(),
            workspace_root,
            sandbox_owned,
            workspace_root_ready: remote.workspace_root_ready,
            workspace_projection_hash: remote.workspace_projection_hash.clone(),
            extra,
            workspace_spec,
            snapshot: remote.snapshot.clone(),
        })
    }
}

/// Holds the harness state cache alongside the typed remote data.
#[derive(Debug, Clone)]
pub struct RemoteSandboxCore {
    /// Typed state data.
    pub data: RemoteSandboxStateData,
    /// Whether the sandbox is currently running.
    pub running: bool,
    state_cache: SandboxState,
}

impl RemoteSandboxCore {
    /// Wraps typed data and initializes the harness state cache.
    pub fn new(data: RemoteSandboxStateData) -> Self {
        let state_cache = data.to_harness_state();
        Self {
            data,
            running: false,
            state_cache,
        }
    }

    /// Returns the cached harness state.
    pub fn state(&self) -> &SandboxState {
        &self.state_cache
    }

    /// Rebuilds the cached harness state from the typed data.
    pub fn sync(&mut self) {
        self.state_cache = self.data.to_harness_state();
    }

    /// Returns whether the sandbox is running.
    pub fn is_running(&self) -> bool {
        self.running
    }
}

/// Backend-specific lifecycle + exec contract adapted onto the harness.
#[async_trait::async_trait]
pub trait RemoteBackend: Send + Sync + std::fmt::Debug {
    /// Stable backend discriminator (`e2b`, `daytona`, `agentrun`, `kubernetes`).
    fn backend_name(&self) -> &'static str;

    /// Base URL / endpoint recorded in the serialized state.
    fn endpoint_base(&self) -> String;

    /// Absolute workspace root inside the remote sandbox.
    fn workspace_root(&self) -> &str;

    /// Creates a new remote sandbox and returns its handle.
    async fn create_sandbox(&self, session_id: &str) -> SandboxResult<RemoteSandboxHandle>;

    /// Connects to an existing remote sandbox, verifying it is alive.
    async fn connect_sandbox(&self, sandbox_id: &str) -> SandboxResult<RemoteSandboxHandle>;

    /// Destroys the remote sandbox (best effort; 404 is treated as success).
    async fn kill_sandbox(&self, sandbox_id: &str) -> SandboxResult<()>;

    /// Runs a shell command inside the sandbox.
    async fn exec_in(
        &self,
        handle: &RemoteSandboxHandle,
        command: &str,
        cwd: Option<&str>,
        timeout_seconds: u64,
    ) -> SandboxResult<RemoteExecOutput>;

    /// Archives the workspace root, returning the raw tar bytes (default: `tar | base64`).
    async fn persist_workspace_bytes(
        &self,
        handle: &RemoteSandboxHandle,
        root: &str,
    ) -> SandboxResult<Vec<u8>> {
        persist_tar(self, handle, root).await
    }

    /// Restores the workspace root from tar bytes (default: base64 chunk upload + extract).
    ///
    /// Returns `Some(handle)` when the backend had to recreate the sandbox (e.g. restoring an
    /// E2B native snapshot materializes a new sandbox id); the caller adopts the new handle.
    async fn hydrate_workspace_bytes(
        &self,
        handle: &RemoteSandboxHandle,
        root: &str,
        archive: &[u8],
    ) -> SandboxResult<Option<RemoteSandboxHandle>> {
        hydrate_tar(self, handle, root, archive).await?;
        Ok(None)
    }

    /// Native single-file transfer adapter for a live sandbox, when the backend supports one.
    ///
    /// Mirrors the Java `Sandbox implements SandboxFileTransfer` capability probe: backends that
    /// expose a runtime file API (Kubernetes, E2B's envd `/files`) return an adapter, and the
    /// filesystem adapter prefers it over base64-over-exec for paths it accepts. Backends without
    /// a file API keep the default `None`.
    async fn file_transfer(
        &self,
        _handle: &RemoteSandboxHandle,
    ) -> Option<Arc<dyn SandboxFileTransfer>> {
        None
    }
}

/// Converts an exec output into an error when it exited non-zero.
pub fn ensure_output_success(context: &str, output: &RemoteExecOutput) -> SandboxResult<()> {
    crate::workspace::ensure_success(context, &output.to_exec_result())
}

/// Builds the command that tars the workspace root onto stdout, base64-encoded.
pub fn build_tar_base64_command(root: &str) -> String {
    format!("tar -cf - -C {} . | base64 -w0", shell_quote(root))
}

/// Builds the command that tars the workspace root onto stdout (binary).
pub fn build_tar_command(root: &str) -> String {
    format!("tar -cf - -C {} .", shell_quote(root))
}

/// Splits a base64 string into fixed-size chunks (mirrors the Java loop).
pub fn chunk_base64(encoded: &str, chunk_size: usize) -> Vec<String> {
    if chunk_size == 0 {
        return vec![encoded.to_string()];
    }
    encoded
        .as_bytes()
        .chunks(chunk_size)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect()
}

/// Builds the python one-liner appending one base64 chunk to `path`.
pub fn build_b64_append_script(path: &str, chunk: &str) -> String {
    let path_lit = serde_json::to_string(path).unwrap_or_else(|_| "\"\"".to_string());
    let chunk_lit = serde_json::to_string(chunk).unwrap_or_else(|_| "\"\"".to_string());
    let py = format!("import pathlib; pathlib.Path({path_lit}).open('a').write({chunk_lit})");
    format!("python3 -c {}", shell_quote(&py))
}

/// Builds the python one-liner that decodes `path` and extracts the tar into `target_dir`.
pub fn build_b64_extract_script(target_dir: &str, path: &str) -> String {
    let dir_lit = serde_json::to_string(target_dir).unwrap_or_else(|_| "\".\"".to_string());
    let path_lit = serde_json::to_string(path).unwrap_or_else(|_| "\"\"".to_string());
    let py = format!(
        "import base64,pathlib,subprocess; raw=base64.standard_b64decode(pathlib.Path({path_lit}).read_text()); subprocess.run(['tar','xf','-','-C',{dir_lit}],input=raw,check=True)"
    );
    format!("python3 -c {}", shell_quote(&py))
}

/// Computes a stable hash of the workspace spec used to detect projection changes.
pub fn workspace_projection_hash(spec: &WorkspaceSpec) -> String {
    let serialized = serde_json::to_string(spec).unwrap_or_default();
    format!("{:016x}", short_hash(&serialized))
}

/// Archives `root` from a backend into raw tar bytes via the default `tar | base64` path.
pub async fn persist_tar<B: RemoteBackend + ?Sized>(
    backend: &B,
    handle: &RemoteSandboxHandle,
    root: &str,
) -> SandboxResult<Vec<u8>> {
    use base64::Engine as _;
    let output = backend
        .exec_in(
            handle,
            &build_tar_base64_command(root),
            None,
            TAR_TIMEOUT_SECONDS,
        )
        .await?;
    ensure_output_success("workspace archive", &output)?;
    let compact: String = output
        .stdout
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(compact.as_bytes())
        .map_err(|e| {
            SandboxExtensionError::Runtime(
                SandboxErrorCode::CommandExecutionError,
                format!("failed to decode workspace archive base64: {e}"),
            )
        })
}

/// Restores raw tar bytes into `root` via the default base64 chunk upload + extract path.
pub async fn hydrate_tar<B: RemoteBackend + ?Sized>(
    backend: &B,
    handle: &RemoteSandboxHandle,
    root: &str,
    archive: &[u8],
) -> SandboxResult<()> {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(archive);
    let _ = backend
        .exec_in(handle, &format!("rm -f {WS_B64_TMP_PATH}"), None, 30)
        .await?;
    for chunk in chunk_base64(&encoded, B64_CHUNK) {
        let script = build_b64_append_script(WS_B64_TMP_PATH, &chunk);
        let output = backend.exec_in(handle, &script, None, 120).await?;
        ensure_output_success("workspace hydrate chunk", &output)?;
    }
    let extract = build_b64_extract_script(root, WS_B64_TMP_PATH);
    let output = backend
        .exec_in(handle, &extract, None, TAR_TIMEOUT_SECONDS)
        .await?;
    ensure_output_success("workspace hydrate extract", &output)?;
    let _ = backend
        .exec_in(handle, &format!("rm -f {WS_B64_TMP_PATH}"), None, 30)
        .await;
    Ok(())
}

/// Generic harness [`Sandbox`] driving any [`RemoteBackend`].
#[derive(Debug)]
pub struct RemoteSandbox {
    backend: Arc<dyn RemoteBackend>,
    core: RemoteSandboxCore,
    handle: Option<RemoteSandboxHandle>,
    snapshot: Option<Arc<dyn SandboxSnapshot>>,
    /// Native file-transfer adapter for the current handle, refreshed whenever a handle is
    /// (re)established. `None` when the backend has no file API.
    transfer: Option<Arc<dyn SandboxFileTransfer>>,
}

impl RemoteSandbox {
    /// Creates a sandbox bound to `backend` with the given state and optional snapshot.
    pub fn new(
        backend: Arc<dyn RemoteBackend>,
        data: RemoteSandboxStateData,
        snapshot: Option<Arc<dyn SandboxSnapshot>>,
    ) -> Self {
        let handle = data
            .sandbox_id
            .as_ref()
            .map(|id| RemoteSandboxHandle::new(id.clone(), data.extra.clone()));
        Self {
            backend,
            core: RemoteSandboxCore::new(data),
            handle,
            snapshot,
            transfer: None,
        }
    }

    /// Returns the backend name.
    pub fn backend_name(&self) -> &'static str {
        self.backend.backend_name()
    }

    /// Native file-transfer adapter for the live sandbox, when the backend exposes one.
    ///
    /// Mirrors the Java `Sandbox instanceof SandboxFileTransfer` probe used by
    /// `SandboxBackedFilesystem`: callers pass the result to
    /// [`crate::fs_spec::SandboxFilesystemAdapter::with_file_transfer`] so reads/writes inside the
    /// runtime file API's scope skip base64-over-exec. The adapter is refreshed on every
    /// (re)establish of the sandbox handle, so it always targets the live sandbox.
    pub fn file_transfer(&self) -> Option<Arc<dyn SandboxFileTransfer>> {
        self.transfer.clone()
    }

    fn workspace_spec(&self) -> WorkspaceSpec {
        self.core
            .data
            .workspace_spec
            .clone()
            .unwrap_or_else(|| WorkspaceSpec::new(self.core.data.workspace_root.clone()))
    }

    async fn ensure_handle(&mut self) -> Result<&RemoteSandboxHandle> {
        if self.handle.is_none() {
            let handle = self
                .backend
                .create_sandbox(&self.core.data.session_id)
                .await?;
            self.adopt_handle(handle);
            self.core.data.workspace_root_ready = false;
            self.core.data.workspace_projection_hash = None;
        } else {
            let current = self
                .handle
                .as_ref()
                .map(|h| h.id.clone())
                .unwrap_or_default();
            if let Err(e) = self.backend.connect_sandbox(&current).await {
                tracing::warn!(
                    "sandbox[{}] connect to {current} failed ({e}); recreating",
                    self.backend.backend_name()
                );
                let handle = self
                    .backend
                    .create_sandbox(&self.core.data.session_id)
                    .await?;
                self.adopt_handle(handle);
                self.core.data.workspace_root_ready = false;
                self.core.data.workspace_projection_hash = None;
            }
        }
        self.core.sync();
        self.refresh_file_transfer().await;
        Ok(self.handle.as_ref().expect("handle established"))
    }

    /// Rebuilds the native file-transfer adapter from the current handle.
    async fn refresh_file_transfer(&mut self) {
        self.transfer = match self.handle.as_ref() {
            Some(handle) => self.backend.file_transfer(handle).await,
            None => None,
        };
    }

    fn adopt_handle(&mut self, handle: RemoteSandboxHandle) {
        self.core.data.sandbox_id = Some(handle.id.clone());
        if !handle.extra.is_empty() {
            for (k, v) in &handle.extra {
                self.core.data.extra.insert(k.clone(), v.clone());
            }
        }
        self.handle = Some(handle);
    }
}

#[async_trait::async_trait]
impl Sandbox for RemoteSandbox {
    async fn start(&mut self) -> Result<()> {
        let handle = self.ensure_handle().await?.clone();
        let root = self.core.data.workspace_root.clone();

        let mut restored = false;
        if let Some(snapshot) = self.snapshot.clone() {
            match snapshot.is_restorable().await {
                Ok(true) => {
                    let archive = snapshot.restore().await?;
                    if let Some(new_handle) = self
                        .backend
                        .hydrate_workspace_bytes(&handle, &root, &archive)
                        .await?
                    {
                        self.adopt_handle(new_handle);
                        self.refresh_file_transfer().await;
                    }
                    restored = true;
                }
                Ok(false) => {}
                Err(e) => tracing::warn!("sandbox: snapshot restorability check failed: {e}"),
            }
        }

        if !restored {
            let spec = self.workspace_spec();
            let script = build_workspace_setup_script(&spec);
            let output = self.backend.exec_in(&handle, &script, None, 120).await?;
            ensure_output_success("workspace setup", &output)?;
            self.core.data.workspace_projection_hash = Some(workspace_projection_hash(&spec));
        }

        self.core.data.workspace_root_ready = true;
        self.core.running = true;
        self.core.sync();
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        let handle = self
            .handle
            .clone()
            .ok_or_else(|| Error::Other("sandbox not started".to_string()))?;
        let root = self.core.data.workspace_root.clone();
        let archive = self.backend.persist_workspace_bytes(&handle, &root).await?;
        if let Some(snapshot) = self.snapshot.clone() {
            snapshot.persist(&archive).await?;
        }
        self.core.data.workspace_root_ready = true;
        self.core.running = false;
        self.core.sync();
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        if self.core.data.sandbox_owned {
            if let Some(id) = self.handle.as_ref().map(|h| h.id.clone()) {
                self.backend.kill_sandbox(&id).await?;
            }
        }
        self.handle = None;
        self.core.data.sandbox_id = None;
        self.core.running = false;
        self.core.data.workspace_root_ready = false;
        self.core.sync();
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.core.is_running()
    }

    fn state(&self) -> &SandboxState {
        self.core.state()
    }

    async fn exec(
        &self,
        _runtime_context: Option<&agentscope_core::agent::RuntimeContext>,
        command: &str,
        timeout_seconds: Option<u64>,
    ) -> Result<ExecResult> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| Error::Other("sandbox not started".to_string()))?;
        let output = self
            .backend
            .exec_in(handle, command, None, timeout_seconds.unwrap_or(120))
            .await?;
        Ok(output.to_exec_result())
    }

    async fn persist_workspace(&self) -> Result<Vec<u8>> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| Error::Other("sandbox not started".to_string()))?;
        let root = self.core.data.workspace_root.clone();
        Ok(self.backend.persist_workspace_bytes(handle, &root).await?)
    }

    async fn hydrate_workspace(&self, archive: &[u8]) -> Result<()> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| Error::Other("sandbox not started".to_string()))?;
        let root = self.core.data.workspace_root.clone();
        let _ = self
            .backend
            .hydrate_workspace_bytes(handle, &root, archive)
            .await?;
        Ok(())
    }
}

/// Generic harness [`SandboxClient`] factory for any [`RemoteBackend`].
#[derive(Debug)]
pub struct RemoteSandboxClient {
    backend: Arc<dyn RemoteBackend>,
    snapshot_spec: Option<Arc<dyn SandboxSnapshotSpec>>,
}

impl RemoteSandboxClient {
    /// Creates a client for the given backend.
    pub fn new(backend: Arc<dyn RemoteBackend>) -> Self {
        Self {
            backend,
            snapshot_spec: None,
        }
    }

    /// Attaches a default snapshot spec applied when `create` receives none.
    pub fn with_snapshot_spec(mut self, spec: Arc<dyn SandboxSnapshotSpec>) -> Self {
        self.snapshot_spec = Some(spec);
        self
    }

    /// Returns the backend discriminator.
    pub fn backend_name(&self) -> &'static str {
        self.backend.backend_name()
    }
}

#[async_trait::async_trait]
impl SandboxClient for RemoteSandboxClient {
    async fn create(
        &self,
        workspace_spec: &WorkspaceSpec,
        snapshot_spec: Option<&dyn SandboxSnapshotSpec>,
        _options: Option<&SandboxClientOptions>,
    ) -> Result<Box<dyn Sandbox>> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let spec = snapshot_spec.or(self.snapshot_spec.as_deref());
        let snapshot = match spec {
            Some(spec) => Some(spec.build(&session_id)?),
            None => None,
        };
        let data = RemoteSandboxStateData {
            session_id,
            backend: self.backend.backend_name().to_string(),
            sandbox_id: None,
            endpoint: self.backend.endpoint_base(),
            token: None,
            workspace_root: self.backend.workspace_root().to_string(),
            sandbox_owned: true,
            workspace_root_ready: false,
            workspace_projection_hash: None,
            extra: BTreeMap::new(),
            workspace_spec: Some(workspace_spec.clone()),
            // Record the identity so a persisted state can be cascaded back to the snapshot
            // (MONGODB `cascadeDeleteSnapshots` reads exactly this field).
            snapshot: snapshot
                .as_ref()
                .map(|s| SandboxSnapshotRef::from_snapshot(s.as_ref())),
        };
        Ok(Box::new(RemoteSandbox::new(
            self.backend.clone(),
            data,
            snapshot,
        )))
    }

    async fn resume(&self, state: &SandboxState) -> Result<Box<dyn Sandbox>> {
        let data = RemoteSandboxStateData::from_harness_state(state)?;
        Ok(Box::new(RemoteSandbox::new(
            self.backend.clone(),
            data,
            None,
        )))
    }

    async fn delete(&self, _sandbox: &dyn Sandbox) -> Result<()> {
        // Mirrors the Java clients, which leave deletion to `Sandbox::shutdown`.
        Ok(())
    }

    fn serialize_state(&self, state: &SandboxState) -> Result<String> {
        serde_json::to_string(state).map_err(Error::from)
    }

    fn deserialize_state(&self, json: &str) -> Result<SandboxState> {
        serde_json::from_str(json).map_err(Error::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn sample_data() -> RemoteSandboxStateData {
        let mut extra = BTreeMap::new();
        extra.insert("template_id".to_string(), "base".to_string());
        RemoteSandboxStateData {
            session_id: "sess-1".to_string(),
            backend: "e2b".to_string(),
            sandbox_id: Some("sbx-1".to_string()),
            endpoint: "https://api.e2b.app".to_string(),
            token: Some("tok".to_string()),
            workspace_root: "/home/user".to_string(),
            sandbox_owned: true,
            workspace_root_ready: true,
            workspace_projection_hash: Some("abc".to_string()),
            extra,
            workspace_spec: Some(WorkspaceSpec::new("/home/user").with_file("a.txt", "hi")),
            snapshot: None,
        }
    }

    #[test]
    fn state_projects_into_remote_variant() {
        let data = sample_data();
        let state = data.to_harness_state();
        match &state {
            SandboxState::Remote(remote) => {
                assert_eq!(remote.session_id, "sess-1");
                assert_eq!(remote.endpoint, "https://api.e2b.app");
                assert_eq!(remote.token.as_deref(), Some("tok"));
                assert!(remote.workspace_root_ready);
                assert_eq!(
                    remote
                        .metadata
                        .get(BACKEND_METADATA_KEY)
                        .map(String::as_str),
                    Some("e2b")
                );
            }
            other => panic!("expected remote state, got {other:?}"),
        }
    }

    #[test]
    fn state_round_trips_through_harness() {
        let data = sample_data();
        let state = data.to_harness_state();
        let restored = RemoteSandboxStateData::from_harness_state(&state).unwrap();
        assert_eq!(restored.backend, "e2b");
        assert_eq!(restored.workspace_root, "/home/user");
        assert_eq!(
            restored.extra.get("template_id").map(String::as_str),
            Some("base")
        );
        assert!(restored.workspace_spec.is_some());
        assert_eq!(restored.session_id, "sess-1");
    }

    #[test]
    fn state_rejects_non_remote_variants() {
        let local = SandboxState::Local(agentscope_harness::agent::sandbox::LocalSandboxState {
            session_id: "l".to_string(),
            temp_dir: "/tmp/l".to_string(),
            workspace_root_ready: false,
            workspace_projection_hash: None,
            metadata: Default::default(),
            snapshot: None,
        });
        assert!(RemoteSandboxStateData::from_harness_state(&local).is_err());
    }

    #[test]
    fn base64_chunks_cover_input() {
        let chunks = chunk_base64("abcdefg", 3);
        assert_eq!(chunks, vec!["abc", "def", "g"]);
        assert_eq!(chunks.concat(), "abcdefg");
    }

    #[test]
    fn append_and_extract_scripts_are_well_formed() {
        let append = build_b64_append_script("/tmp/x.b64", "QUJD");
        assert!(append.starts_with("python3 -c "));
        assert!(append.contains("/tmp/x.b64"));
        assert!(append.contains("QUJD"));
        let extract = build_b64_extract_script("/workspace", "/tmp/x.b64");
        assert!(extract.contains("standard_b64decode"));
        assert!(extract.contains("/workspace"));
    }

    #[test]
    fn tar_command_quotes_root() {
        assert_eq!(
            build_tar_base64_command("/home/user"),
            "tar -cf - -C /home/user . | base64 -w0"
        );
        assert!(build_tar_command("/with space").contains("'/with space'"));
    }

    #[test]
    fn projection_hash_is_stable() {
        let spec = WorkspaceSpec::new("/workspace").with_file("a", "b");
        assert_eq!(
            workspace_projection_hash(&spec),
            workspace_projection_hash(&spec)
        );
        let other = WorkspaceSpec::new("/workspace").with_file("a", "c");
        assert_ne!(
            workspace_projection_hash(&spec),
            workspace_projection_hash(&other)
        );
    }

    #[test]
    fn exec_output_converts_to_harness_result() {
        let out = RemoteExecOutput::new(3, "o".into(), "e".into());
        let result = out.to_exec_result();
        assert_eq!(result.exit_code, 3);
        assert_eq!(result.combined_output(), "o\ne");
        assert!(ensure_output_success("ctx", &out).is_err());
        let ok = RemoteExecOutput::new(0, "ok".into(), String::new());
        assert!(ensure_output_success("ctx", &ok).is_ok());
    }

    // A minimal in-memory backend used to exercise the generic `Sandbox` lifecycle offline.
    #[derive(Debug)]
    struct FakeBackend {
        commands: Mutex<Vec<String>>,
        alive: Mutex<bool>,
        native_transfer: bool,
    }

    /// Stand-in native file-transfer adapter (see the harness-gap note in `kubernetes_files`).
    #[derive(Debug)]
    struct StubTransfer;

    #[async_trait::async_trait]
    impl crate::kubernetes_files::SandboxFileTransfer for StubTransfer {
        fn supports_file_transfer(&self, _absolute_path: &str) -> bool {
            true
        }
        async fn upload_file(&self, _absolute_path: &str, _content: &[u8]) -> SandboxResult<()> {
            Ok(())
        }
        async fn download_file(&self, _absolute_path: &str) -> SandboxResult<Vec<u8>> {
            Ok(Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl RemoteBackend for FakeBackend {
        fn backend_name(&self) -> &'static str {
            "fake"
        }
        fn endpoint_base(&self) -> String {
            "memory://fake".to_string()
        }
        fn workspace_root(&self) -> &str {
            "/workspace"
        }
        async fn create_sandbox(&self, _session_id: &str) -> SandboxResult<RemoteSandboxHandle> {
            *self.alive.lock().unwrap() = true;
            Ok(RemoteSandboxHandle::new("fake-1", BTreeMap::new()))
        }
        async fn connect_sandbox(&self, sandbox_id: &str) -> SandboxResult<RemoteSandboxHandle> {
            if *self.alive.lock().unwrap() {
                Ok(RemoteSandboxHandle::new(sandbox_id, BTreeMap::new()))
            } else {
                Err(SandboxExtensionError::Transport("dead".into()))
            }
        }
        async fn kill_sandbox(&self, _sandbox_id: &str) -> SandboxResult<()> {
            *self.alive.lock().unwrap() = false;
            Ok(())
        }
        async fn file_transfer(
            &self,
            _handle: &RemoteSandboxHandle,
        ) -> Option<Arc<dyn crate::kubernetes_files::SandboxFileTransfer>> {
            self.native_transfer
                .then(|| Arc::new(StubTransfer) as Arc<_>)
        }
        async fn exec_in(
            &self,
            _handle: &RemoteSandboxHandle,
            command: &str,
            _cwd: Option<&str>,
            _timeout_seconds: u64,
        ) -> SandboxResult<RemoteExecOutput> {
            self.commands.lock().unwrap().push(command.to_string());
            // Emulate `tar | base64` for an empty archive so `stop` can round-trip.
            if command.contains("base64 -w0") {
                Ok(RemoteExecOutput::new(0, String::new(), String::new()))
            } else {
                Ok(RemoteExecOutput::new(0, String::new(), String::new()))
            }
        }
    }

    #[tokio::test]
    async fn lifecycle_start_exec_stop_shutdown() {
        let backend = Arc::new(FakeBackend {
            commands: Mutex::new(Vec::new()),
            alive: Mutex::new(false),
            native_transfer: false,
        });
        let client = RemoteSandboxClient::new(backend.clone());
        let spec = WorkspaceSpec::new("/workspace").with_file("README.md", "hi");
        let mut sandbox = client.create(&spec, None, None).await.unwrap();
        assert!(!sandbox.is_running());
        sandbox.start().await.unwrap();
        assert!(sandbox.is_running());
        assert_eq!(sandbox.state().session_id(), sandbox.state().session_id());
        let result = sandbox.exec(None, "echo hi", Some(5)).await.unwrap();
        assert_eq!(result.exit_code, 0);
        sandbox.stop().await.unwrap();
        assert!(!sandbox.is_running());
        sandbox.shutdown().await.unwrap();
        assert!(!*backend.alive.lock().unwrap());
        let recorded = backend.commands.lock().unwrap().clone();
        assert!(recorded.iter().any(|c| c.contains("mkdir -p /workspace")));
        assert!(recorded.iter().any(|c| c.contains("echo hi")));
    }

    #[tokio::test]
    async fn create_records_snapshot_identity_in_state() {
        #[derive(Debug)]
        struct FakeRemoteSnapshotClient;

        #[async_trait::async_trait]
        impl agentscope_harness::agent::sandbox::RemoteSnapshotClient for FakeRemoteSnapshotClient {
            async fn upload(&self, _snapshot_id: &str, _data: &[u8]) -> Result<()> {
                Ok(())
            }
            async fn download(&self, _snapshot_id: &str) -> Result<Vec<u8>> {
                Ok(Vec::new())
            }
            async fn exists(&self, _snapshot_id: &str) -> Result<bool> {
                Ok(false)
            }
        }

        let backend = Arc::new(FakeBackend {
            commands: Mutex::new(Vec::new()),
            alive: Mutex::new(false),
            native_transfer: false,
        });
        let client = RemoteSandboxClient::new(backend).with_snapshot_spec(Arc::new(
            agentscope_harness::agent::sandbox::RemoteSnapshotSpec::new(Arc::new(
                FakeRemoteSnapshotClient,
            )),
        ));
        let spec = WorkspaceSpec::new("/workspace");
        let sandbox = client.create(&spec, None, None).await.unwrap();

        let state = sandbox.state();
        let snapshot_id = match state.snapshot() {
            Some(SandboxSnapshotRef::Remote { id }) => id.clone(),
            other => panic!("expected a remote snapshot ref, got {other:?}"),
        };
        assert_eq!(snapshot_id, state.session_id());

        // The typed carrier round-trips the identity, so a persisted state keeps it.
        let data = RemoteSandboxStateData::from_harness_state(&state).unwrap();
        assert_eq!(data.snapshot, state.snapshot().cloned());
        assert_eq!(
            data.to_harness_state().snapshot(),
            state.snapshot().cloned().as_ref()
        );
    }

    #[tokio::test]
    async fn resume_reads_remote_state() {
        let backend = Arc::new(FakeBackend {
            commands: Mutex::new(Vec::new()),
            alive: Mutex::new(true),
            native_transfer: false,
        });
        let client = RemoteSandboxClient::new(backend);
        let state = sample_data().to_harness_state();
        let sandbox = client.resume(&state).await.unwrap();
        assert_eq!(sandbox.state().session_id(), "sess-1");
    }

    #[tokio::test]
    async fn serialize_state_round_trips() {
        let backend = Arc::new(FakeBackend {
            commands: Mutex::new(Vec::new()),
            alive: Mutex::new(false),
            native_transfer: false,
        });
        let client = RemoteSandboxClient::new(backend);
        let json = client
            .serialize_state(&sample_data().to_harness_state())
            .unwrap();
        let parsed = client.deserialize_state(&json).unwrap();
        assert_eq!(parsed.session_id(), "sess-1");
    }

    #[tokio::test]
    async fn native_file_transfer_is_published_after_the_handle_is_established() {
        let backend = Arc::new(FakeBackend {
            commands: Mutex::new(Vec::new()),
            alive: Mutex::new(true),
            native_transfer: true,
        });
        let mut sandbox = RemoteSandbox::new(backend.clone(), sample_data(), None);
        // The adapter is refreshed in `ensure_handle`, so it is absent until the sandbox starts.
        assert!(sandbox.file_transfer().is_none());
        sandbox.start().await.unwrap();
        assert!(sandbox.file_transfer().is_some());

        // Backends without a runtime file API keep the exec-only strategy.
        let backend = Arc::new(FakeBackend {
            commands: Mutex::new(Vec::new()),
            alive: Mutex::new(true),
            native_transfer: false,
        });
        let mut plain = RemoteSandbox::new(backend, sample_data(), None);
        plain.start().await.unwrap();
        assert!(plain.file_transfer().is_none());
    }
}
