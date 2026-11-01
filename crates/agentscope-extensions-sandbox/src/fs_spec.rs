//! Declarative sandbox-filesystem specs.
//!
//! Java models these as subclasses of the harness `SandboxFilesystemSpec` (an abstract
//! builder that turns a client + workspace/snapshot spec into a `SandboxContext`). The Rust
//! harness has **no** `SandboxFilesystemSpec` type: it exposes only the runtime
//! [`SandboxFilesystem`] trait and the local `SandboxBackedFilesystem` helper. Rather than
//! modifying the harness (out of scope for this crate), this module defines a small local
//! equivalent — [`SandboxFilesystemSpec`] — that captures the same declarative surface
//! (client + workspace projection + snapshot override + isolation scope + execution guard) and an
//! adapter ([`SandboxFilesystemAdapter`]) implementing the harness [`SandboxFilesystem`] trait
//! through a live [`Sandbox`].
//!
//! Workspace projection mirrors the Java `SandboxFilesystemSpec` exactly: when projection is
//! enabled and a host workspace root is supplied, [`SandboxFilesystemSpec::layout_spec`] writes a
//! [`WorkspaceProjectionEntry`] under [`WORKSPACE_PROJECTION_ENTRY_KEY`] into the harness
//! [`LayoutSpec`] entry map, and [`SandboxFilesystemSpec::projection_payload`] turns it into the
//! archive the sandbox hydrates at start (harness `WorkspaceProjectionApplier`).
//!
//! Design decision (tracker §4.2): the spec lives in this crate, not in the harness. If the
//! harness later gains a `SandboxFilesystemSpec`, the four `*FilesystemSpec` types below can be
//! switched to implement/extend it without touching call sites, because they already expose the
//! `effective_workspace_spec` / `effective_snapshot_spec` / `build_context` surface.

use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::sandbox::{
    LayoutSpec, NoopSnapshotSpec, ProjectionPayload, Sandbox, SandboxClient, SandboxClientOptions,
    SandboxContext, SandboxGuard, SandboxSnapshotSpec, SandboxState, WorkspaceProjectionApplier,
    WorkspaceProjectionEntry, WorkspaceSpec,
};
use agentscope_harness::agent::IsolationScope;
use agentscope_harness::filesystem::{
    ExecuteResponse, FileData, FileInfo, Filesystem, FilesystemKind, GrepMatch, SandboxFilesystem,
};
use async_trait::async_trait;

use crate::agentrun::{agentrun_client, AgentRunSandboxClientOptions};
use crate::daytona::{daytona_client, DaytonaSandboxClientOptions};
use crate::e2b::{e2b_client, E2bSandboxClientOptions};
use crate::error::{SandboxErrorCode, SandboxExtensionError, SandboxResult};
use crate::kubernetes::{kubernetes_client, KubernetesSandboxClientOptions};
use crate::kubernetes_files::SandboxFileTransfer;
use crate::remote::RemoteSandbox;
use crate::workspace::shell_quote;

/// Default workspace-projection entries copied into the sandbox workspace.
///
/// Mirrors the Java `SandboxFilesystemSpec.DEFAULT_WORKSPACE_PROJECTION_ROOTS`.
pub const DEFAULT_WORKSPACE_PROJECTION_ROOTS: [&str; 5] = [
    "AGENTS.md",
    "skills",
    "subagents",
    "knowledge",
    ".skills-cache",
];

/// Projection roots that are directories (created eagerly in the default workspace spec).
pub const DEFAULT_WORKSPACE_PROJECTION_DIRS: [&str; 4] =
    ["skills", "subagents", "knowledge", ".skills-cache"];

/// Event name used to carry a projected entry inside a workspace spec.
///
/// Mirrors the Java `SandboxFilesystemSpec` entry key: the projection entry is stored in the
/// layout entry map under this name. Rust keeps the materialization-oriented `WorkspaceSpec`
/// typed (root / directories / files), so the entry lives in the harness [`LayoutSpec`] returned
/// by [`SandboxFilesystemSpec::layout_spec`], where the harness `WorkspaceProjectionApplier` picks
/// it up.
pub const WORKSPACE_PROJECTION_ENTRY_KEY: &str = "__workspace_projection__";

/// Returns the default projection roots as owned strings (Java
/// `SandboxFilesystemSpec.DEFAULT_WORKSPACE_PROJECTION_ROOTS`).
pub fn default_workspace_projection_roots() -> Vec<String> {
    DEFAULT_WORKSPACE_PROJECTION_ROOTS
        .iter()
        .map(|root| (*root).to_string())
        .collect()
}

/// Absolutises `root` without touching the filesystem.
///
/// Mirrors the Java `hostWorkspaceRoot.toAbsolutePath().normalize()` call: relative roots are
/// joined onto the current directory, and `.`/`..` segments are folded lexically (the host
/// workspace may not exist yet, so `canonicalize` is not usable).
fn absolute_path(root: &Path) -> PathBuf {
    let joined = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(root))
            .unwrap_or_else(|_| root.to_path_buf())
    };
    let mut folded = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                folded.pop();
            }
            other => folded.push(other.as_os_str()),
        }
    }
    folded
}

/// Builds the default workspace spec: root plus the standard projection directories.
pub fn default_workspace_spec(root: &str) -> WorkspaceSpec {
    let mut spec = WorkspaceSpec::new(root);
    for entry in DEFAULT_WORKSPACE_PROJECTION_DIRS {
        spec.directories.push(entry.to_string());
    }
    spec
}

/// A declarative description of a sandbox-backed filesystem.
///
/// Rust stand-in for the Java harness `SandboxFilesystemSpec`. Concrete platform specs
/// ([`AgentRunFilesystemSpec`], [`DaytonaFilesystemSpec`], [`E2bFilesystemSpec`],
/// [`KubernetesFilesystemSpec`]) fill in the client / options hooks.
#[async_trait]
pub trait SandboxFilesystemSpec: Debug + Send + Sync {
    /// Stable platform discriminator (`agentrun` / `daytona` / `e2b` / `kubernetes`).
    fn platform(&self) -> &'static str;

    /// Creates the harness [`SandboxClient`] for this platform (validating options eagerly).
    fn create_client(&self) -> SandboxResult<Box<dyn SandboxClient>>;

    /// Returns the platform-specific client options rendered as a stable debug string.
    ///
    /// Remote platforms keep their own options structs because the harness
    /// [`SandboxClientOptions`] only carries Docker knobs; this hook exposes a portable view
    /// for diagnostics.
    fn client_options_debug(&self) -> String;

    /// Default workspace spec (root + projection).
    fn workspace_spec(&self) -> WorkspaceSpec;

    /// Returns the effective snapshot spec (override wins over the platform default).
    fn effective_snapshot_spec(&self) -> Arc<dyn SandboxSnapshotSpec>;

    /// Returns the effective workspace spec (override wins over the platform default).
    fn effective_workspace_spec(&self) -> WorkspaceSpec;

    /// Returns the configured isolation scope, if any.
    fn isolation_scope(&self) -> Option<IsolationScope>;

    /// Returns the execution guard applied to sandbox executions, if any.
    ///
    /// Mirrors the Java `SandboxFilesystemSpec#getExecutionGuard`. The guard is **retained**, not
    /// applied here: hosts wire it into
    /// `SandboxManager::with_shared_guard(spec.execution_guard().unwrap_or(noop))`, exactly where
    /// Java passes it to `new SandboxManager(..., executionGuard)`.
    fn execution_guard(&self) -> Option<Arc<dyn SandboxGuard>> {
        None
    }

    /// Whether host workspace projection is enabled (Java
    /// `workspaceProjectionEnabled`, default `true`).
    fn workspace_projection_enabled(&self) -> bool {
        true
    }

    /// Roots projected from the host workspace (Java `workspaceProjectionRoots`, default
    /// [`DEFAULT_WORKSPACE_PROJECTION_ROOTS`]).
    fn workspace_projection_roots(&self) -> Vec<String> {
        default_workspace_projection_roots()
    }

    /// Builds the `__workspace_projection__` entry for `host_workspace_root`, if projection is on.
    ///
    /// Mirrors the Java `buildWorkspaceSpecWithProjection`: the source root is absolutised and the
    /// include roots are the configured projection roots. `None` when projection is disabled or no
    /// host workspace root is supplied (the Java `toSandboxContext()` overload passes `null`).
    fn workspace_projection_entry(
        &self,
        host_workspace_root: Option<&Path>,
    ) -> Option<(String, WorkspaceProjectionEntry)> {
        if !self.workspace_projection_enabled() {
            return None;
        }
        let host = host_workspace_root?;
        Some((
            WORKSPACE_PROJECTION_ENTRY_KEY.to_string(),
            WorkspaceProjectionEntry {
                ephemeral: false,
                source_root: absolute_path(host).to_string_lossy().replace('\\', "/"),
                include_roots: self.workspace_projection_roots(),
            },
        ))
    }

    /// Returns the harness [`LayoutSpec`] for this platform.
    ///
    /// This is the Rust analogue of the Java `WorkspaceSpec` entries map: when a host workspace
    /// root is supplied and projection is enabled, the map really contains the
    /// [`WORKSPACE_PROJECTION_ENTRY_KEY`] entry, so the harness `WorkspaceProjectionApplier` can
    /// turn it into an archive at sandbox start.
    fn layout_spec(&self, host_workspace_root: Option<&Path>) -> LayoutSpec {
        let root = self
            .workspace_spec()
            .root
            .to_string_lossy()
            .replace('\\', "/");
        let mut layout = LayoutSpec::new().with_root(root);
        if let Some((key, entry)) = self.workspace_projection_entry(host_workspace_root) {
            layout = layout.entry(key, entry);
        }
        layout
    }

    /// Builds the projection archive for `host_workspace_root`.
    ///
    /// Delegates to the harness `WorkspaceProjectionApplier`, the Rust counterpart of the Java
    /// archive hydration performed inside `Sandbox#start()`. `Ok(None)` when projection is
    /// disabled or nothing matches the include roots.
    fn projection_payload(
        &self,
        host_workspace_root: &Path,
    ) -> SandboxResult<Option<ProjectionPayload>> {
        WorkspaceProjectionApplier::build(&self.layout_spec(Some(host_workspace_root))).map_err(
            |e| {
                SandboxExtensionError::Runtime(
                    SandboxErrorCode::WorkspaceStartError,
                    format!("workspace projection failed: {e}"),
                )
            },
        )
    }

    /// Builds the harness [`SandboxContext`] plus the workspace-projection payload.
    ///
    /// Mirrors the Java `toSandboxContext(Path hostWorkspaceRoot)` call site in `HarnessAgent`:
    /// the projection entry is written into the layout spec under
    /// [`WORKSPACE_PROJECTION_ENTRY_KEY`] and the payload is what the sandbox hydrates at start.
    fn build_context_with_projection(
        &self,
        host_workspace_root: &Path,
    ) -> SandboxResult<(SandboxContext, Option<ProjectionPayload>)> {
        let context = self.build_context()?;
        let payload = self.projection_payload(host_workspace_root)?;
        Ok((context, payload))
    }

    /// Overrides the snapshot spec (builder-style).
    fn with_snapshot_spec(self, spec: Arc<dyn SandboxSnapshotSpec>) -> Self
    where
        Self: Sized;

    /// Overrides the workspace spec (builder-style).
    fn with_workspace_spec(self, spec: WorkspaceSpec) -> Self
    where
        Self: Sized;

    /// Sets the isolation scope (builder-style).
    fn with_isolation_scope(self, scope: IsolationScope) -> Self
    where
        Self: Sized;

    /// Builds the harness [`SandboxContext`] for this spec.
    ///
    /// Mirrors `SandboxFilesystemSpec#toSandboxContext`: creates the client, applies the
    /// workspace/snapshot specs and records the isolation scope.
    fn build_context(&self) -> SandboxResult<SandboxContext> {
        let client = self.create_client()?;
        let mut builder = SandboxContext::builder()
            .client(client)
            .workspace_spec(self.effective_workspace_spec())
            .snapshot_spec(self.effective_snapshot_spec());
        if let Some(scope) = self.isolation_scope() {
            builder = builder.isolation_scope(scope);
        }
        Ok(builder.build())
    }

    /// Creates a live harness [`Sandbox`] for `workspace_spec` (already started).
    async fn create_sandbox(
        &self,
        workspace_spec: &WorkspaceSpec,
    ) -> SandboxResult<Box<dyn Sandbox>> {
        let client = self.create_client()?;
        let spec = self.effective_snapshot_spec();
        let mut sandbox = client
            .create(workspace_spec, Some(spec.as_ref()), None)
            .await
            .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
        sandbox
            .start()
            .await
            .map_err(|e| SandboxExtensionError::Transport(e.to_string()))?;
        Ok(sandbox)
    }

    /// Builds a harness [`SandboxFilesystem`] bound to a live sandbox.
    fn filesystem(&self, sandbox: Arc<dyn Sandbox>, cwd: &str) -> Arc<dyn SandboxFilesystem> {
        self.filesystem_with_transfer(sandbox, None, cwd)
    }

    /// Builds a harness [`SandboxFilesystem`] that prefers native file transfer.
    ///
    /// Mirrors the Java `SandboxBackedFilesystem` `instanceof SandboxFileTransfer` branch: when a
    /// transfer adapter is supplied, paths it accepts are read/written through it and only the
    /// remaining paths fall back to base64-over-exec. Callers obtain the adapter from
    /// [`RemoteSandbox::file_transfer`] (or build one from a backend directly).
    fn filesystem_with_transfer(
        &self,
        sandbox: Arc<dyn Sandbox>,
        transfer: Option<Arc<dyn SandboxFileTransfer>>,
        cwd: &str,
    ) -> Arc<dyn SandboxFilesystem> {
        let root = self
            .effective_workspace_spec()
            .root
            .to_string_lossy()
            .replace('\\', "/");
        Arc::new(SandboxFilesystemAdapter::new_with_transfer(
            sandbox,
            root,
            cwd.to_string(),
            transfer,
        ))
    }
}

/// An externally supplied [`SandboxClient`] (mirrors the Java `client(...)` hook).
///
/// `dyn SandboxClient` is not `Debug`, so this newtype provides a manual `Debug` impl and a
/// delegating [`SandboxClient`] implementation so it can be boxed like any other client.
#[derive(Clone)]
pub struct ClientHandle {
    inner: Arc<dyn SandboxClient>,
}

impl ClientHandle {
    /// Wraps an existing client.
    pub fn new(client: Arc<dyn SandboxClient>) -> Self {
        Self { inner: client }
    }
}

impl Debug for ClientHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientHandle")
    }
}

#[async_trait]
impl SandboxClient for ClientHandle {
    async fn create(
        &self,
        workspace_spec: &WorkspaceSpec,
        snapshot_spec: Option<&dyn SandboxSnapshotSpec>,
        options: Option<&SandboxClientOptions>,
    ) -> Result<Box<dyn Sandbox>> {
        self.inner
            .create(workspace_spec, snapshot_spec, options)
            .await
    }

    async fn resume(&self, state: &SandboxState) -> Result<Box<dyn Sandbox>> {
        self.inner.resume(state).await
    }

    async fn delete(&self, sandbox: &dyn Sandbox) -> Result<()> {
        self.inner.delete(sandbox).await
    }

    fn serialize_state(&self, state: &SandboxState) -> Result<String> {
        self.inner.serialize_state(state)
    }

    fn deserialize_state(&self, json: &str) -> Result<SandboxState> {
        self.inner.deserialize_state(json)
    }
}

/// Adapter projecting a live harness [`Sandbox`] onto the harness [`SandboxFilesystem`] trait.
///
/// The harness provides `SandboxBackedFilesystem`, but that helper wraps an inner *filesystem*
/// and runs shell commands on the **local host**. A remote sandbox has no local filesystem to
/// delegate to, so every operation is expressed as a shell command run through
/// [`Sandbox::exec`] — the same strategy the Java `SandboxBackedFilesystem` uses once a live
/// sandbox is bound. When a [`SandboxFileTransfer`] adapter is attached, `read`/`write` (and
/// therefore `edit`) prefer it for paths it accepts and fall back to the shell strategy
/// otherwise, matching the Java `instanceof SandboxFileTransfer` branch.
#[derive(Debug)]
pub struct SandboxFilesystemAdapter {
    sandbox: Arc<dyn Sandbox>,
    workspace_root: String,
    cwd: String,
    /// Native file-transfer adapter, when the backend exposes a runtime file API.
    ///
    /// Mirrors the Java `SandboxBackedFilesystem` `instanceof SandboxFileTransfer` probe: paths
    /// the adapter accepts bypass base64-over-exec entirely.
    transfer: Option<Arc<dyn SandboxFileTransfer>>,
}

impl SandboxFilesystemAdapter {
    /// Creates an adapter over a live sandbox (exec-only, no native transfer).
    pub fn new(
        sandbox: Arc<dyn Sandbox>,
        workspace_root: impl Into<String>,
        cwd: impl Into<String>,
    ) -> Self {
        Self::new_with_transfer(sandbox, workspace_root, cwd, None)
    }

    /// Creates an adapter that prefers `transfer` for the paths it accepts.
    pub fn new_with_transfer(
        sandbox: Arc<dyn Sandbox>,
        workspace_root: impl Into<String>,
        cwd: impl Into<String>,
        transfer: Option<Arc<dyn SandboxFileTransfer>>,
    ) -> Self {
        Self {
            sandbox,
            workspace_root: workspace_root.into(),
            cwd: cwd.into(),
            transfer,
        }
    }

    /// Attaches a native file-transfer adapter (builder style).
    pub fn with_file_transfer(mut self, transfer: Arc<dyn SandboxFileTransfer>) -> Self {
        self.transfer = Some(transfer);
        self
    }

    /// Returns the attached native file-transfer adapter, if any.
    pub fn file_transfer(&self) -> Option<&Arc<dyn SandboxFileTransfer>> {
        self.transfer.as_ref()
    }

    /// Returns a live sandbox paired with its native transfer adapter as a filesystem.
    ///
    /// This is the Rust equivalent of the Java `SandboxBackedFilesystem` auto-detection: the
    /// adapter asks the sandbox for its backend-provided transfer capability at construction
    /// time.
    pub fn from_remote_sandbox(
        sandbox: Arc<RemoteSandbox>,
        workspace_root: impl Into<String>,
        cwd: impl Into<String>,
    ) -> Self {
        let transfer = sandbox.file_transfer();
        let sandbox: Arc<dyn Sandbox> = sandbox;
        Self {
            sandbox,
            workspace_root: workspace_root.into(),
            cwd: cwd.into(),
            transfer,
        }
    }

    /// Returns the wrapped sandbox.
    pub fn sandbox(&self) -> &Arc<dyn Sandbox> {
        &self.sandbox
    }

    /// Returns the workspace root used to resolve relative paths.
    pub fn workspace_root(&self) -> &str {
        &self.workspace_root
    }

    /// Resolves an absolute remote path, rooting relative paths at the workspace root.
    pub fn resolve_path(&self, path: &str) -> String {
        let trimmed = path.trim();
        if trimmed.is_empty() || trimmed == "." {
            return self.workspace_root.trim_end_matches('/').to_string();
        }
        if trimmed.starts_with('/') {
            return trimmed.to_string();
        }
        format!("{}/{}", self.workspace_root.trim_end_matches('/'), trimmed)
    }

    fn cwd_or_root(&self) -> String {
        if self.cwd.trim().is_empty() || self.cwd == "." {
            self.workspace_root.clone()
        } else if self.cwd.starts_with('/') {
            self.cwd.clone()
        } else {
            format!("{}/{}", self.workspace_root.trim_end_matches('/'), self.cwd)
        }
    }

    async fn run(&self, command: &str, timeout_seconds: u64) -> Result<ExecuteResponse> {
        let result = self
            .sandbox
            .exec(None, command, Some(timeout_seconds))
            .await?;
        Ok(ExecuteResponse {
            output: result.combined_output(),
            exit_code: result.exit_code,
            truncated: result.truncated,
        })
    }

    async fn run_checked(
        &self,
        command: &str,
        context: &str,
        timeout_seconds: u64,
    ) -> Result<String> {
        let response = self.run(command, timeout_seconds).await?;
        if response.exit_code != 0 {
            return Err(Error::Tool(format!(
                "sandbox {context} failed (exit {}): {}",
                response.exit_code, response.output
            )));
        }
        Ok(response.output)
    }
}

/// Parses `find -printf '%y\t%s\t%p\n'` output into [`FileInfo`] entries.
///
/// Lines that do not match the expected shape are skipped rather than failing the whole call.
pub fn parse_ls_output(output: &str, workspace_root: &str) -> Vec<FileInfo> {
    let mut out = Vec::new();
    let root = workspace_root.trim_end_matches('/');
    for line in output.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, '\t');
        let kind = parts.next().unwrap_or("");
        let size = parts.next().unwrap_or("0").parse::<u64>().unwrap_or(0);
        let path = match parts.next() {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => continue,
        };
        let relative = path
            .strip_prefix(&format!("{root}/"))
            .unwrap_or(&path)
            .to_string();
        if kind == "d" {
            out.push(FileInfo::dir(relative));
        } else {
            out.push(FileInfo::file(relative, size));
        }
    }
    out
}

/// Builds the `ls` command: one `find` line per entry (`type<TAB>size<TAB>path`).
pub fn build_ls_command(absolute_path: &str) -> String {
    format!(
        "find {} -maxdepth 1 -mindepth 1 -printf '%y\\t%s\\t%p\\n' 2>/dev/null",
        shell_quote(absolute_path)
    )
}

/// Builds the `read` command with 1-based line offset/limit (matching the harness semantics).
pub fn build_read_command(absolute_path: &str, offset: usize, limit: usize) -> String {
    if limit > 0 {
        let start = offset + 1;
        let end = offset + limit;
        format!("sed -n '{start},{end}p' {}", shell_quote(absolute_path))
    } else {
        let start = offset + 1;
        format!("tail -n +{start} {}", shell_quote(absolute_path))
    }
}

/// Parses `grep -rn` output (`path:line:text`) into [`GrepMatch`] entries.
pub fn parse_grep_output(output: &str) -> Vec<GrepMatch> {
    let mut matches = Vec::new();
    for line in output.lines() {
        let line = line.trim_end_matches('\r');
        let text = line.to_string();
        let mut split = line.splitn(3, ':');
        let (Some(path), Some(lineno), Some(_rest)) = (split.next(), split.next(), split.next())
        else {
            continue;
        };
        let Ok(line_number) = lineno.parse::<usize>() else {
            continue;
        };
        matches.push(GrepMatch {
            path: path.to_string(),
            line: line_number,
            text,
        });
    }
    matches
}

/// Applies the `sed -n 'start,endp'` line window to already-downloaded text.
///
/// `offset` is 0-based and `limit == 0` means "to the end". Lines keep their original
/// terminators, so the result is byte-identical to what the exec path returns for the same
/// window (a trailing newline is preserved, a final unterminated line stays unterminated).
pub fn apply_line_window(content: &str, offset: usize, limit: usize) -> String {
    let mut out = String::new();
    let mut emitted = 0usize;
    for (index, line) in content.split_inclusive('\n').enumerate() {
        if index < offset {
            continue;
        }
        if limit > 0 && emitted >= limit {
            break;
        }
        out.push_str(line);
        emitted += 1;
    }
    out
}

/// Projects downloaded bytes onto a harness [`FileData`] honouring the read window.
///
/// Binary payloads (NUL bytes) are returned base64-encoded in full, mirroring the exec path
/// which cannot window binary data meaningfully.
fn file_data_from_bytes(bytes: &[u8], offset: usize, limit: usize) -> FileData {
    if bytes.contains(&0) {
        use base64::Engine as _;
        FileData::binary(base64::engine::general_purpose::STANDARD.encode(bytes))
    } else {
        let text = String::from_utf8_lossy(bytes);
        FileData::text(apply_line_window(&text, offset, limit))
    }
}

fn decode_text(data: &FileData) -> Result<String> {
    if data.encoding == "base64" {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.content.as_bytes())
            .map_err(|e| Error::Tool(format!("invalid base64 file content: {e}")))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        Ok(data.content.clone())
    }
}

#[async_trait]
impl SandboxFilesystem for SandboxFilesystemAdapter {
    async fn execute(
        &self,
        _ctx: &RuntimeContext,
        command: &str,
        timeout_seconds: u32,
    ) -> Result<ExecuteResponse> {
        // Subshell delimiters go on their own lines: a trailing `)` glued to the last
        // payload line breaks heredocs (the shell would see `EOF)` instead of `EOF`).
        // Mirrors the Java fix in `KubernetesSandbox#doExec` (commit c767160b1).
        let wrapped = format!("cd {} && (\n{command}\n)", shell_quote(&self.cwd_or_root()));
        self.run(&wrapped, u64::from(timeout_seconds)).await
    }
}

#[async_trait]
impl Filesystem for SandboxFilesystemAdapter {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn kind(&self) -> FilesystemKind {
        FilesystemKind::Sandbox
    }

    async fn ls(&self, _ctx: &RuntimeContext, path: &str) -> Result<Vec<FileInfo>> {
        let absolute = self.resolve_path(path);
        let output = self
            .run_checked(&build_ls_command(&absolute), "ls", 60)
            .await?;
        Ok(parse_ls_output(&output, &self.workspace_root))
    }

    async fn read(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<FileData> {
        let absolute = self.resolve_path(path);
        if let Some(transfer) = self.transfer.as_ref() {
            if transfer.supports_file_transfer(&absolute) {
                let bytes = transfer
                    .download_file(&absolute)
                    .await
                    .map_err(|e| Error::Tool(format!("sandbox read failed for '{path}': {e}")))?;
                return Ok(file_data_from_bytes(&bytes, offset, limit));
            }
        }
        if !self.exists(ctx, &absolute).await? {
            return Err(Error::Tool(format!("File '{path}' not found")));
        }
        let command = build_read_command(&absolute, offset, limit);
        let output = self.run_checked(&command, "read", 60).await?;
        if output.as_bytes().contains(&0) {
            use base64::Engine as _;
            let encoded = base64::engine::general_purpose::STANDARD.encode(output.as_bytes());
            Ok(FileData::binary(encoded))
        } else {
            Ok(FileData::text(output))
        }
    }

    async fn write(&self, _ctx: &RuntimeContext, path: &str, content: &str) -> Result<()> {
        let absolute = self.resolve_path(path);
        if let Some(transfer) = self.transfer.as_ref() {
            if transfer.supports_file_transfer(&absolute) {
                transfer
                    .upload_file(&absolute, content.as_bytes())
                    .await
                    .map_err(|e| Error::Tool(format!("sandbox write failed for '{path}': {e}")))?;
                return Ok(());
            }
        }
        if let Some(parent) = crate::workspace::parent_dir(&absolute) {
            self.run_checked(&format!("mkdir -p {}", shell_quote(&parent)), "mkdir", 60)
                .await?;
        }
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
        let script = format!(
            "printf %s {} | base64 -d > {}",
            shell_quote(&encoded),
            shell_quote(&absolute)
        );
        self.run_checked(&script, "write", 60).await?;
        Ok(())
    }

    async fn edit(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<usize> {
        if old.is_empty() {
            return Err(Error::Tool("old_string must not be empty".into()));
        }
        let current = self.read(ctx, path, 0, 0).await?;
        let text = decode_text(&current)?;
        let count = text.matches(old).count();
        if count == 0 {
            return Err(Error::Tool(format!("old_string not found in {path}")));
        }
        if !replace_all && count > 1 {
            return Err(Error::Tool(format!(
                "old_string is not unique in {path} ({count} matches); pass replace_all=true"
            )));
        }
        let replaced = if replace_all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
        self.write(ctx, path, &replaced).await?;
        Ok(if replace_all { count } else { 1 })
    }

    async fn grep(
        &self,
        _ctx: &RuntimeContext,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>> {
        let target = self.resolve_path(path.unwrap_or("."));
        let include = glob
            .map(|g| format!(" --include={}", shell_quote(g)))
            .unwrap_or_default();
        let command = format!(
            "grep -rn{include} -e {} {} 2>/dev/null",
            shell_quote(pattern),
            shell_quote(&target)
        );
        let response = self.run(&command, 60).await?;
        // grep exits 1 on "no matches" — not an error for the caller.
        if response.exit_code > 1 {
            return Err(Error::Tool(format!(
                "sandbox grep failed (exit {}): {}",
                response.exit_code, response.output
            )));
        }
        Ok(parse_grep_output(&response.output))
    }

    async fn glob(
        &self,
        _ctx: &RuntimeContext,
        pattern: &str,
        path: &str,
    ) -> Result<Vec<FileInfo>> {
        let target = self.resolve_path(path);
        let command = format!(
            "find {} -name {} -printf '%y\\t%s\\t%p\\n' 2>/dev/null",
            shell_quote(&target),
            shell_quote(pattern)
        );
        let response = self.run(&command, 60).await?;
        if response.exit_code > 1 {
            return Err(Error::Tool(format!(
                "sandbox glob failed (exit {}): {}",
                response.exit_code, response.output
            )));
        }
        Ok(parse_ls_output(&response.output, &self.workspace_root))
    }

    async fn delete(&self, _ctx: &RuntimeContext, path: &str) -> Result<()> {
        let absolute = self.resolve_path(path);
        self.run_checked(&format!("rm -rf {}", shell_quote(&absolute)), "delete", 60)
            .await?;
        Ok(())
    }

    async fn move_path(&self, _ctx: &RuntimeContext, from: &str, to: &str) -> Result<()> {
        let src = self.resolve_path(from);
        let dst = self.resolve_path(to);
        if let Some(parent) = crate::workspace::parent_dir(&dst) {
            self.run_checked(&format!("mkdir -p {}", shell_quote(&parent)), "mkdir", 60)
                .await?;
        }
        self.run_checked(
            &format!("mv {} {}", shell_quote(&src), shell_quote(&dst)),
            "move",
            60,
        )
        .await?;
        Ok(())
    }

    async fn exists(&self, _ctx: &RuntimeContext, path: &str) -> Result<bool> {
        let absolute = self.resolve_path(path);
        let response = self
            .run(&format!("test -e {}", shell_quote(&absolute)), 30)
            .await?;
        Ok(response.exit_code == 0)
    }
}

/// Shared builder state reused by every platform spec.
#[derive(Debug, Clone)]
struct SpecCommon {
    snapshot_override: Option<Arc<dyn SandboxSnapshotSpec>>,
    workspace_override: Option<WorkspaceSpec>,
    isolation_scope: Option<IsolationScope>,
    /// Guard retained for the host (Java `executionGuard`).
    execution_guard: Option<Arc<dyn SandboxGuard>>,
    /// Whether host workspace projection is enabled (Java default: `true`).
    workspace_projection_enabled: bool,
    /// Roots projected from the host workspace (Java `workspaceProjectionRoots`).
    workspace_projection_roots: Vec<String>,
}

impl Default for SpecCommon {
    fn default() -> Self {
        Self {
            snapshot_override: None,
            workspace_override: None,
            isolation_scope: None,
            execution_guard: None,
            workspace_projection_enabled: true,
            workspace_projection_roots: default_workspace_projection_roots(),
        }
    }
}

impl SpecCommon {
    fn effective_snapshot(&self) -> Arc<dyn SandboxSnapshotSpec> {
        self.snapshot_override
            .clone()
            .unwrap_or_else(|| Arc::new(NoopSnapshotSpec::new()))
    }

    fn effective_workspace(&self, root: &str) -> WorkspaceSpec {
        self.workspace_override
            .clone()
            .unwrap_or_else(|| default_workspace_spec(root))
    }
}

macro_rules! platform_spec {
    ($name:ident, $platform:literal, $options:ty, $client_fn:path, $root_of:ident, $docs:literal) => {
        #[doc = $docs]
        #[derive(Debug, Clone)]
        pub struct $name {
            options: $options,
            common: SpecCommon,
            extra_roots: Vec<String>,
            client_override: Option<ClientHandle>,
        }

        impl $name {
            /// Creates a spec with platform-default options.
            pub fn new(options: $options) -> Self {
                Self {
                    options,
                    common: SpecCommon::default(),
                    extra_roots: Vec::new(),
                    client_override: None,
                }
            }

            /// Returns the platform options.
            pub fn options(&self) -> &$options {
                &self.options
            }

            /// Returns mutable platform options.
            pub fn options_mut(&mut self) -> &mut $options {
                &mut self.options
            }

            /// Injects a pre-built client (mirrors the Java `client(SandboxClient)` hook).
            pub fn client(mut self, client: Arc<dyn SandboxClient>) -> Self {
                self.client_override = Some(ClientHandle::new(client));
                self
            }

            /// Adds an extra workspace directory to create (not a Java
            /// `workspaceProjectionRoots` entry — see `workspace_projection_roots`).
            pub fn add_projection_root(mut self, entry: impl Into<String>) -> Self {
                self.extra_roots.push(entry.into());
                self
            }

            /// Returns the extra workspace directories (see `add_projection_root`).
            pub fn projection_roots(&self) -> &[String] {
                &self.extra_roots
            }

            /// Sets the execution guard (mirrors the Java `executionGuard(...)`).
            ///
            /// The guard is retained on the spec so the host can hand it to
            /// `SandboxManager::with_shared_guard`; read it back with
            /// [`SandboxFilesystemSpec::execution_guard`].
            pub fn with_execution_guard(mut self, guard: Arc<dyn SandboxGuard>) -> Self {
                self.common.execution_guard = Some(guard);
                self
            }

            /// Enables or disables host workspace projection (mirrors the Java
            /// `workspaceProjectionEnabled(...)`; default `true`).
            pub fn with_workspace_projection_enabled(mut self, enabled: bool) -> Self {
                self.common.workspace_projection_enabled = enabled;
                self
            }

            /// Replaces the projected include roots (mirrors the Java
            /// `workspaceProjectionRoots(...)`; an empty list projects nothing).
            pub fn with_workspace_projection_roots<I, S>(mut self, roots: I) -> Self
            where
                I: IntoIterator<Item = S>,
                S: Into<String>,
            {
                self.common.workspace_projection_roots =
                    roots.into_iter().map(Into::into).collect();
                self
            }

            /// Returns the workspace root reported by the platform options.
            pub fn workspace_root(&self) -> String {
                $root_of(&self.options)
            }
        }

        impl SandboxFilesystemSpec for $name {
            fn platform(&self) -> &'static str {
                $platform
            }

            fn create_client(&self) -> SandboxResult<Box<dyn SandboxClient>> {
                if let Some(client) = &self.client_override {
                    return Ok(Box::new(client.clone()));
                }
                Ok(Box::new(($client_fn)(self.options.clone())?))
            }

            fn client_options_debug(&self) -> String {
                format!("{:?}", self.options)
            }

            fn workspace_spec(&self) -> WorkspaceSpec {
                default_workspace_spec(&self.workspace_root())
            }

            fn effective_snapshot_spec(&self) -> Arc<dyn SandboxSnapshotSpec> {
                self.common.effective_snapshot()
            }

            fn effective_workspace_spec(&self) -> WorkspaceSpec {
                let mut spec = self.common.effective_workspace(&self.workspace_root());
                for extra in &self.extra_roots {
                    spec.directories.push(extra.clone());
                }
                spec
            }

            fn isolation_scope(&self) -> Option<IsolationScope> {
                self.common.isolation_scope
            }

            fn execution_guard(&self) -> Option<Arc<dyn SandboxGuard>> {
                self.common.execution_guard.clone()
            }

            fn workspace_projection_enabled(&self) -> bool {
                self.common.workspace_projection_enabled
            }

            fn workspace_projection_roots(&self) -> Vec<String> {
                self.common.workspace_projection_roots.clone()
            }

            fn with_snapshot_spec(mut self, spec: Arc<dyn SandboxSnapshotSpec>) -> Self {
                self.common.snapshot_override = Some(spec);
                self
            }

            fn with_workspace_spec(mut self, spec: WorkspaceSpec) -> Self {
                self.common.workspace_override = Some(spec);
                self
            }

            fn with_isolation_scope(mut self, scope: IsolationScope) -> Self {
                self.common.isolation_scope = Some(scope);
                self
            }
        }
    };
}

fn agentrun_root(options: &AgentRunSandboxClientOptions) -> String {
    options.workspace_root.clone()
}
fn daytona_root(options: &DaytonaSandboxClientOptions) -> String {
    options.workspace_root.clone()
}
fn e2b_root(options: &E2bSandboxClientOptions) -> String {
    options.workspace_root.clone()
}
fn kubernetes_root(options: &KubernetesSandboxClientOptions) -> String {
    options.workspace_root.clone()
}

platform_spec!(
    AgentRunFilesystemSpec,
    "agentrun",
    AgentRunSandboxClientOptions,
    agentrun_client,
    agentrun_root,
    "`SandboxFilesystemSpec` for the Alibaba Cloud AgentRun sandbox backend."
);
platform_spec!(
    DaytonaFilesystemSpec,
    "daytona",
    DaytonaSandboxClientOptions,
    daytona_client,
    daytona_root,
    "`SandboxFilesystemSpec` for the Daytona sandbox backend."
);
platform_spec!(
    E2bFilesystemSpec,
    "e2b",
    E2bSandboxClientOptions,
    e2b_client,
    e2b_root,
    "`SandboxFilesystemSpec` for the E2B cloud sandbox backend."
);
platform_spec!(
    KubernetesFilesystemSpec,
    "kubernetes",
    KubernetesSandboxClientOptions,
    kubernetes_client,
    kubernetes_root,
    "`SandboxFilesystemSpec` for the agent-sandbox Kubernetes backend."
);

impl E2bFilesystemSpec {
    /// Registers a sandbox-creation environment variable (Java `E2bFilesystemSpec#env`).
    ///
    /// Blank names are rejected, mirroring the Java option setter.
    pub fn env(self, name: impl Into<String>, value: impl Into<String>) -> SandboxResult<Self> {
        let mut spec = self;
        spec.options = spec.options.with_env(name, value)?;
        Ok(spec)
    }

    /// Mounts a pre-existing E2B volume (Java `E2bFilesystemSpec#volumeMount`).
    ///
    /// The mount is validated eagerly (absolute in-sandbox path, distinct from the other mounts)
    /// and threaded into the `POST /sandboxes` body by the backend.
    pub fn volume_mount(
        self,
        name: impl Into<String>,
        path: impl Into<String>,
    ) -> SandboxResult<Self> {
        let mut spec = self;
        spec.options = spec.options.with_volume_mount(name, path)?;
        Ok(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_harness::agent::sandbox::{
        ExecResult, SandboxExecutionGuard, SandboxGuard, SandboxIsolationKey, WorkspaceEntry,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn shell_quote_handles_spaces() {
        assert_eq!(shell_quote("plain"), "plain");
        assert_eq!(shell_quote("with space"), "'with space'");
    }

    #[test]
    fn default_workspace_spec_includes_projection_dirs() {
        let spec = default_workspace_spec("/workspace");
        assert_eq!(spec.root, PathBuf::from("/workspace"));
        assert!(spec.directories.contains(&"skills".to_string()));
        assert!(spec.directories.contains(&"subagents".to_string()));
        assert!(spec.directories.contains(&"knowledge".to_string()));
        assert!(spec.directories.contains(&".skills-cache".to_string()));
        assert!(!spec.directories.contains(&"AGENTS.md".to_string()));
    }

    #[test]
    fn resolve_path_roots_relative() {
        let sandbox = Arc::new(FakeSandbox::default());
        let fs = SandboxFilesystemAdapter::new(sandbox, "/workspace", ".");
        assert_eq!(fs.resolve_path("a/b.txt"), "/workspace/a/b.txt");
        assert_eq!(fs.resolve_path("/etc/hosts"), "/etc/hosts");
        assert_eq!(fs.resolve_path("."), "/workspace");
        assert_eq!(fs.workspace_root(), "/workspace");
    }

    #[test]
    fn parse_ls_output_reads_type_and_size() {
        let raw = "d\t4096\t/workspace/dir\nf\t12\t/workspace/f.txt\n";
        let entries = parse_ls_output(raw, "/workspace");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "dir");
        assert!(entries[0].is_directory);
        assert_eq!(entries[1].path, "f.txt");
        assert_eq!(entries[1].size, 12);
    }

    #[test]
    fn parse_ls_output_skips_malformed_lines() {
        let entries = parse_ls_output("garbage\n\n", "/workspace");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_grep_output_skips_malformed_lines() {
        let raw = "a.txt:3:hello\nnot-a-match\nb.txt:notanum:x\n";
        let matches = parse_grep_output(raw);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "a.txt");
        assert_eq!(matches[0].line, 3);
        assert_eq!(matches[0].text, "a.txt:3:hello");
    }

    #[test]
    fn read_command_uses_line_window() {
        assert_eq!(
            build_read_command("/w/f.txt", 0, 10),
            "sed -n '1,10p' /w/f.txt"
        );
        assert_eq!(
            build_read_command("/w/f.txt", 5, 5),
            "sed -n '6,10p' /w/f.txt"
        );
        assert_eq!(build_read_command("/w/f.txt", 2, 0), "tail -n +3 /w/f.txt");
    }

    #[test]
    fn ls_command_is_quoted() {
        assert!(build_ls_command("/w with space").contains("'/w with space'"));
    }

    #[test]
    fn platform_specs_report_distinct_names() {
        let specs: Vec<Box<dyn SandboxFilesystemSpec>> = vec![
            Box::new(AgentRunFilesystemSpec::new(
                AgentRunSandboxClientOptions::default(),
            )),
            Box::new(DaytonaFilesystemSpec::new(
                DaytonaSandboxClientOptions::default(),
            )),
            Box::new(E2bFilesystemSpec::new(E2bSandboxClientOptions::default())),
            Box::new(KubernetesFilesystemSpec::new(
                KubernetesSandboxClientOptions::default(),
            )),
        ];
        let names: Vec<_> = specs.iter().map(|s| s.platform()).collect();
        assert_eq!(names, vec!["agentrun", "daytona", "e2b", "kubernetes"]);
    }

    #[test]
    fn agentrun_spec_exposes_options_from_java_setters() {
        let mut spec = AgentRunFilesystemSpec::new(AgentRunSandboxClientOptions::default());
        spec.options_mut().api_key = Some("k".to_string());
        spec.options_mut().template_name = Some("tpl".to_string());
        assert_eq!(spec.options().api_key.as_deref(), Some("k"));
        assert_eq!(spec.options().template_name.as_deref(), Some("tpl"));
        assert!(spec
            .client_options_debug()
            .contains("AgentRunSandboxClientOptions"));
    }

    #[test]
    fn workspace_override_wins_over_default() {
        let custom = WorkspaceSpec::new("/custom");
        let spec =
            E2bFilesystemSpec::new(E2bSandboxClientOptions::default()).with_workspace_spec(custom);
        assert_eq!(
            spec.effective_workspace_spec().root,
            PathBuf::from("/custom")
        );
        assert!(spec.effective_workspace_spec().directories.is_empty());
    }

    #[test]
    fn extra_projection_roots_are_appended() {
        let spec = DaytonaFilesystemSpec::new(DaytonaSandboxClientOptions::default())
            .add_projection_root("extra");
        assert_eq!(spec.projection_roots(), &["extra".to_string()]);
        assert!(spec
            .effective_workspace_spec()
            .directories
            .contains(&"extra".to_string()));
    }

    #[test]
    fn isolation_scope_is_recorded() {
        let spec = DaytonaFilesystemSpec::new(DaytonaSandboxClientOptions::default())
            .with_isolation_scope(IsolationScope::Session);
        assert_eq!(spec.isolation_scope(), Some(IsolationScope::Session));
    }

    #[test]
    fn e2b_default_root_matches_options() {
        let options = E2bSandboxClientOptions::default();
        let spec = E2bFilesystemSpec::new(options);
        assert_eq!(spec.workspace_root(), spec.options().workspace_root);
    }

    #[test]
    fn build_context_records_workspace_and_scope() {
        let spec = KubernetesFilesystemSpec::new(KubernetesSandboxClientOptions::default())
            .with_isolation_scope(IsolationScope::Agent);
        let context = spec.build_context().expect("context builds");
        assert!(context.client().is_some());
        assert_eq!(context.isolation_scope(), Some(&IsolationScope::Agent));
        assert!(context.workspace_spec().is_some());
    }

    #[test]
    fn projection_entry_is_written_when_enabled() {
        let spec = E2bFilesystemSpec::new(E2bSandboxClientOptions::default());
        // No host root -> no entry (Java `toSandboxContext()` passes null).
        assert!(spec.layout_spec(None).entries.is_empty());

        let layout = spec.layout_spec(Some(Path::new("host-workspace")));
        let entry = layout
            .entries
            .get(WORKSPACE_PROJECTION_ENTRY_KEY)
            .expect("projection entry is written");
        match entry {
            WorkspaceEntry::WorkspaceProjection(projection) => {
                assert_eq!(
                    projection.include_roots,
                    default_workspace_projection_roots()
                );
                assert!(projection.include_roots.contains(&"AGENTS.md".to_string()));
                assert!(projection.source_root.ends_with("host-workspace"));
                assert!(Path::new(&projection.source_root).is_absolute());
                assert!(!projection.ephemeral);
            }
            other => panic!("expected a workspace projection entry, got {other:?}"),
        }
        assert_eq!(layout.root, spec.workspace_root());
    }

    #[test]
    fn projection_entry_is_omitted_when_disabled() {
        let spec = DaytonaFilesystemSpec::new(DaytonaSandboxClientOptions::default())
            .with_workspace_projection_enabled(false);
        assert!(!spec.workspace_projection_enabled());
        assert!(spec
            .layout_spec(Some(Path::new("/host/workspace")))
            .entries
            .is_empty());
        assert!(spec
            .projection_payload(Path::new("/host/workspace"))
            .expect("disabled projection is not an error")
            .is_none());
    }

    #[test]
    fn projection_roots_are_configurable() {
        let spec = E2bFilesystemSpec::new(E2bSandboxClientOptions::default())
            .with_workspace_projection_roots(["AGENTS.md".to_string(), "docs".to_string()]);
        assert_eq!(
            spec.workspace_projection_roots(),
            vec!["AGENTS.md".to_string(), "docs".to_string()]
        );
        let layout = spec.layout_spec(Some(Path::new("/host/ws")));
        match layout.entries.get(WORKSPACE_PROJECTION_ENTRY_KEY).unwrap() {
            WorkspaceEntry::WorkspaceProjection(projection) => {
                assert_eq!(projection.include_roots, vec!["AGENTS.md", "docs"]);
                assert!(projection.source_root.ends_with("host/ws"));
                assert!(!projection.source_root.contains('\\'));
            }
            other => panic!("expected a workspace projection entry, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execution_guard_is_retained() {
        let guard: Arc<dyn SandboxGuard> = Arc::new(SandboxExecutionGuard::new());
        let spec = AgentRunFilesystemSpec::new(AgentRunSandboxClientOptions::default())
            .with_execution_guard(guard);
        let retained = spec.execution_guard().expect("guard is retained");
        let key = SandboxIsolationKey::new("user", "u1");
        let _lease = retained.try_enter(key.clone()).await.expect("lease");
        assert!(retained.is_held(&key));
        retained.release(&key).await;
        assert!(!retained.is_held(&key));
    }

    #[test]
    fn e2b_spec_builders_thread_envs_and_volume_mounts() {
        let spec = E2bFilesystemSpec::new(E2bSandboxClientOptions::default())
            .env("TOKEN", "abc")
            .expect("valid env")
            .volume_mount("vol-a", "/mnt/data")
            .expect("valid mount");
        assert_eq!(
            spec.options().envs.get("TOKEN").map(String::as_str),
            Some("abc")
        );
        assert_eq!(spec.options().volume_mounts.len(), 1);
        assert_eq!(spec.options().volume_mounts[0].path, "/mnt/data");
        // 校验语义与 options 一致：空 env 名与相对挂载路径都要拒绝。
        assert!(E2bFilesystemSpec::new(E2bSandboxClientOptions::default())
            .env("  ", "x")
            .is_err());
        assert!(E2bFilesystemSpec::new(E2bSandboxClientOptions::default())
            .volume_mount("vol-a", "relative")
            .is_err());
    }

    #[test]
    fn every_platform_spec_accepts_guard_and_projection_settings() {
        let guard: Arc<dyn SandboxGuard> = Arc::new(SandboxExecutionGuard::new());
        let specs: Vec<Box<dyn SandboxFilesystemSpec>> = vec![
            Box::new(
                AgentRunFilesystemSpec::new(AgentRunSandboxClientOptions::default())
                    .with_execution_guard(guard.clone())
                    .with_workspace_projection_enabled(true)
                    .with_workspace_projection_roots(["AGENTS.md"]),
            ),
            Box::new(
                DaytonaFilesystemSpec::new(DaytonaSandboxClientOptions::default())
                    .with_execution_guard(guard.clone())
                    .with_workspace_projection_enabled(true)
                    .with_workspace_projection_roots(["AGENTS.md"]),
            ),
            Box::new(
                E2bFilesystemSpec::new(E2bSandboxClientOptions::default())
                    .with_execution_guard(guard.clone())
                    .with_workspace_projection_enabled(true)
                    .with_workspace_projection_roots(["AGENTS.md"]),
            ),
            Box::new(
                KubernetesFilesystemSpec::new(KubernetesSandboxClientOptions::default())
                    .with_execution_guard(guard.clone())
                    .with_workspace_projection_enabled(true)
                    .with_workspace_projection_roots(["AGENTS.md"]),
            ),
        ];
        for spec in &specs {
            assert!(
                spec.execution_guard().is_some(),
                "{} lost its guard",
                spec.platform()
            );
            assert!(spec.workspace_projection_enabled());
            assert_eq!(spec.workspace_projection_roots(), vec!["AGENTS.md"]);
            let layout = spec.layout_spec(Some(Path::new("/host/ws")));
            assert!(layout.entries.contains_key(WORKSPACE_PROJECTION_ENTRY_KEY));
        }
        // Only the Kubernetes spec has usable default options (the other backends require
        // credentials), so the context + payload path is exercised there.
        let (context, payload) =
            KubernetesFilesystemSpec::new(KubernetesSandboxClientOptions::default())
                .with_workspace_projection_roots(["AGENTS.md"])
                .build_context_with_projection(Path::new("/host/ws"))
                .expect("context builds");
        assert!(context.workspace_spec().is_some());
        // `/host/ws` does not exist here, so nothing is projected.
        assert!(payload.is_none());
    }

    #[test]
    fn projection_payload_projects_the_include_roots() {
        let host = std::env::temp_dir().join(format!(
            "agentscope-sandbox-projection-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(host.join("skills")).unwrap();
        std::fs::write(host.join("AGENTS.md"), b"hello agents").unwrap();
        std::fs::write(host.join("skills/skill.md"), b"skill body").unwrap();

        let spec = E2bFilesystemSpec::new(E2bSandboxClientOptions::default())
            .with_workspace_projection_roots(["AGENTS.md", "skills"]);
        let payload = spec
            .projection_payload(&host)
            .expect("projection builds")
            .expect("projection is not empty");
        assert_eq!(payload.file_count, 2);
        assert!(payload.tar_bytes.starts_with(b"AGENTS.md"));

        std::fs::remove_dir_all(&host).unwrap();
    }

    /// Minimal fake sandbox used to exercise the filesystem adapter without a network.
    #[derive(Default)]
    struct FakeSandbox {
        commands: std::sync::Mutex<Vec<String>>,
    }

    impl Debug for FakeSandbox {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("FakeSandbox")
        }
    }

    #[async_trait]
    impl Sandbox for FakeSandbox {
        async fn start(&mut self) -> Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> Result<()> {
            Ok(())
        }
        fn is_running(&self) -> bool {
            true
        }
        fn state(&self) -> &SandboxState {
            unreachable!("FakeSandbox::state is not used by the adapter")
        }
        async fn exec(
            &self,
            _ctx: Option<&RuntimeContext>,
            command: &str,
            _timeout: Option<u64>,
        ) -> Result<ExecResult> {
            self.commands.lock().unwrap().push(command.to_string());
            Ok(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                truncated: false,
            })
        }
        async fn persist_workspace(&self) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn hydrate_workspace(&self, _archive: &[u8]) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn adapter_execute_wraps_command_in_cwd() {
        let sandbox = Arc::new(FakeSandbox::default());
        let fs = SandboxFilesystemAdapter::new(sandbox.clone(), "/workspace", ".");
        let ctx = RuntimeContext::new();
        let response = fs.execute(&ctx, "echo hi", 5).await.unwrap();
        assert_eq!(response.exit_code, 0);
        let seen = sandbox.commands.lock().unwrap().clone();
        assert_eq!(seen, vec!["cd /workspace && (\necho hi\n)".to_string()]);
    }

    #[tokio::test]
    async fn adapter_execute_keeps_closing_paren_off_heredoc_terminator() {
        let sandbox = Arc::new(FakeSandbox::default());
        let fs = SandboxFilesystemAdapter::new(sandbox.clone(), "/workspace", ".");
        let ctx = RuntimeContext::new();
        let command = "cat <<'EOF'\nhello\nEOF";
        fs.execute(&ctx, command, 5).await.unwrap();
        let seen = sandbox.commands.lock().unwrap().clone();
        let wrapped = seen.first().expect("a command was sent");
        assert_eq!(wrapped, &format!("cd /workspace && (\n{command}\n)"));
        // The trailing `)` must sit on its own line, never glued to `EOF`.
        assert!(wrapped.ends_with("\n)"), "wrapped: {wrapped:?}");
        assert!(!wrapped.contains("EOF)"), "wrapped: {wrapped:?}");
    }

    #[tokio::test]
    async fn adapter_exists_uses_test_e() {
        let sandbox = Arc::new(FakeSandbox::default());
        let fs = SandboxFilesystemAdapter::new(sandbox.clone(), "/workspace", ".");
        let ctx = RuntimeContext::new();
        assert!(Filesystem::exists(&fs, &ctx, "a.txt").await.unwrap());
        let seen = sandbox.commands.lock().unwrap().clone();
        assert_eq!(seen, vec!["test -e /workspace/a.txt".to_string()]);
    }

    // -------------------------------- native file transfer preference (Java `instanceof`)

    /// Recording `SandboxFileTransfer` scoped to a path prefix.
    #[derive(Debug)]
    struct FakeTransfer {
        scope: String,
        uploads: std::sync::Mutex<Vec<(String, Vec<u8>)>>,
        content: Vec<u8>,
    }

    impl FakeTransfer {
        fn new(scope: &str, content: &[u8]) -> Self {
            Self {
                scope: scope.to_string(),
                uploads: std::sync::Mutex::new(Vec::new()),
                content: content.to_vec(),
            }
        }
    }

    #[async_trait]
    impl SandboxFileTransfer for FakeTransfer {
        fn supports_file_transfer(&self, absolute_path: &str) -> bool {
            absolute_path.starts_with(&self.scope)
        }

        async fn upload_file(&self, absolute_path: &str, content: &[u8]) -> SandboxResult<()> {
            self.uploads
                .lock()
                .unwrap()
                .push((absolute_path.to_string(), content.to_vec()));
            Ok(())
        }

        async fn download_file(&self, _absolute_path: &str) -> SandboxResult<Vec<u8>> {
            Ok(self.content.clone())
        }
    }

    #[tokio::test]
    async fn adapter_write_prefers_native_transfer() {
        let sandbox = Arc::new(FakeSandbox::default());
        let transfer = Arc::new(FakeTransfer::new("/workspace/", b""));
        let fs = SandboxFilesystemAdapter::new_with_transfer(
            sandbox.clone(),
            "/workspace",
            ".",
            Some(transfer.clone()),
        );
        let ctx = RuntimeContext::new();
        Filesystem::write(&fs, &ctx, "a.txt", "hello")
            .await
            .unwrap();
        let uploads = transfer.uploads.lock().unwrap().clone();
        assert_eq!(
            uploads,
            vec![("/workspace/a.txt".to_string(), b"hello".to_vec())]
        );
        // No mkdir / base64 shell command was needed.
        assert!(sandbox.commands.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn adapter_write_falls_back_to_exec_outside_the_transfer_scope() {
        let sandbox = Arc::new(FakeSandbox::default());
        let transfer = Arc::new(FakeTransfer::new("/workspace/inner/", b""));
        let fs = SandboxFilesystemAdapter::new_with_transfer(
            sandbox.clone(),
            "/workspace",
            ".",
            Some(transfer.clone()),
        );
        let ctx = RuntimeContext::new();
        Filesystem::write(&fs, &ctx, "a.txt", "hello")
            .await
            .unwrap();
        assert!(transfer.uploads.lock().unwrap().is_empty());
        let commands = sandbox.commands.lock().unwrap().clone();
        assert!(
            commands.iter().any(|c| c.contains("base64 -d")),
            "commands: {commands:?}"
        );
    }

    #[tokio::test]
    async fn adapter_read_prefers_native_transfer_and_windows_lines() {
        let sandbox = Arc::new(FakeSandbox::default());
        let transfer = Arc::new(FakeTransfer::new("/workspace/", b"l1\nl2\nl3\n"));
        let fs = SandboxFilesystemAdapter::new_with_transfer(
            sandbox.clone(),
            "/workspace",
            ".",
            Some(transfer),
        );
        let ctx = RuntimeContext::new();
        let data = Filesystem::read(&fs, &ctx, "a.txt", 1, 1).await.unwrap();
        assert_eq!(data.content, "l2\n");
        assert!(sandbox.commands.lock().unwrap().is_empty());
        // limit == 0 reads to the end.
        let rest = Filesystem::read(&fs, &ctx, "a.txt", 1, 0).await.unwrap();
        assert_eq!(rest.content, "l2\nl3\n");
    }

    #[tokio::test]
    async fn adapter_write_creates_parent_without_command_substitution() {
        // Java e190a8d1b: the docker CLI on Windows hosts mangles arguments carrying a
        // double-quoted `"$(dirname ...)"` before the container shell sees them. The parent
        // must be computed here and passed as a single-quoted literal.
        let sandbox = Arc::new(FakeSandbox::default());
        let fs = SandboxFilesystemAdapter::new(sandbox.clone(), "/workspace", ".");
        let ctx = RuntimeContext::new();
        Filesystem::write(&fs, &ctx, "my docs/new file.txt", "hello")
            .await
            .unwrap();
        let commands = sandbox.commands.lock().unwrap().clone();
        assert!(
            commands
                .iter()
                .any(|c| c.contains("mkdir -p '/workspace/my docs'")),
            "parent with spaces must be a single-quoted literal: {commands:?}"
        );
        for command in &commands {
            assert!(
                !command.contains("$("),
                "no command substitution: {command}"
            );
            assert!(!command.contains('"'), "no double quotes: {command}");
        }
    }

    #[tokio::test]
    async fn adapter_move_creates_parent_without_command_substitution() {
        let sandbox = Arc::new(FakeSandbox::default());
        let fs = SandboxFilesystemAdapter::new(sandbox.clone(), "/workspace", ".");
        let ctx = RuntimeContext::new();
        Filesystem::move_path(&fs, &ctx, "a.txt", "my docs/new file.txt")
            .await
            .unwrap();
        let commands = sandbox.commands.lock().unwrap().clone();
        assert!(
            commands
                .iter()
                .any(|c| c.contains("mkdir -p '/workspace/my docs'")),
            "parent with spaces must be a single-quoted literal: {commands:?}"
        );
        for command in &commands {
            assert!(
                !command.contains("$("),
                "no command substitution: {command}"
            );
            assert!(!command.contains('"'), "no double quotes: {command}");
        }
    }

    #[test]
    fn apply_line_window_matches_sed_windows() {
        let text = "a\nb\nc\nd\n";
        assert_eq!(apply_line_window(text, 0, 2), "a\nb\n");
        assert_eq!(apply_line_window(text, 2, 1), "c\n");
        assert_eq!(apply_line_window(text, 3, 0), "d\n");
        assert_eq!(apply_line_window(text, 9, 0), "");
        // An unterminated final line stays unterminated.
        assert_eq!(apply_line_window("a\nb", 1, 1), "b");
    }
}
