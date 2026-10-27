//! Declarative sandbox-filesystem specs.
//!
//! Java models these as subclasses of the harness `SandboxFilesystemSpec` (an abstract
//! builder that turns a client + workspace/snapshot spec into a `SandboxContext`). The Rust
//! harness has **no** `SandboxFilesystemSpec` type: it exposes only the runtime
//! [`SandboxFilesystem`] trait and the local `SandboxBackedFilesystem` helper. Rather than
//! modifying the harness (out of scope for this crate), this module defines a small local
//! equivalent — [`SandboxFilesystemSpec`] — that captures the same declarative surface
//! (client + workspace projection + snapshot override + isolation scope) and an adapter
//! ([`SandboxFilesystemAdapter`]) implementing the harness [`SandboxFilesystem`] trait through
//! a live [`Sandbox`].
//!
//! Design decision (tracker §4.2): the spec lives in this crate, not in the harness. If the
//! harness later gains a `SandboxFilesystemSpec`, the four `*FilesystemSpec` types below can be
//! switched to implement/extend it without touching call sites, because they already expose the
//! `effective_workspace_spec` / `effective_snapshot_spec` / `build_context` surface.

use std::fmt::Debug;
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result};
use agentscope_harness::agent::sandbox::{
    NoopSnapshotSpec, Sandbox, SandboxClient, SandboxClientOptions, SandboxContext,
    SandboxSnapshotSpec, SandboxState, WorkspaceSpec,
};
use agentscope_harness::agent::IsolationScope;
use agentscope_harness::filesystem::{
    ExecuteResponse, FileData, FileInfo, Filesystem, FilesystemKind, GrepMatch, SandboxFilesystem,
};
use async_trait::async_trait;

use crate::agentrun::{agentrun_client, AgentRunSandboxClientOptions};
use crate::daytona::{daytona_client, DaytonaSandboxClientOptions};
use crate::e2b::{e2b_client, E2bSandboxClientOptions};
use crate::error::{SandboxExtensionError, SandboxResult};
use crate::kubernetes::{kubernetes_client, KubernetesSandboxClientOptions};
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

/// Event name used to carry a projected entry inside a Java `WorkspaceSpec`.
///
/// Java stores projection entries in the workspace-spec map under this key. Rust keeps the
/// workspace spec as typed fields, so the include roots are carried as directory entries
/// instead. The constant is retained for parity / diagnostics.
pub const WORKSPACE_PROJECTION_ENTRY_KEY: &str = "__workspace_projection__";

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
        let root = self
            .effective_workspace_spec()
            .root
            .to_string_lossy()
            .replace('\\', "/");
        Arc::new(SandboxFilesystemAdapter::new(
            sandbox,
            root,
            cwd.to_string(),
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
/// sandbox is bound.
#[derive(Debug)]
pub struct SandboxFilesystemAdapter {
    sandbox: Arc<dyn Sandbox>,
    workspace_root: String,
    cwd: String,
}

impl SandboxFilesystemAdapter {
    /// Creates an adapter over a live sandbox.
    pub fn new(
        sandbox: Arc<dyn Sandbox>,
        workspace_root: impl Into<String>,
        cwd: impl Into<String>,
    ) -> Self {
        Self {
            sandbox,
            workspace_root: workspace_root.into(),
            cwd: cwd.into(),
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
#[derive(Debug, Clone, Default)]
struct SpecCommon {
    snapshot_override: Option<Arc<dyn SandboxSnapshotSpec>>,
    workspace_override: Option<WorkspaceSpec>,
    isolation_scope: Option<IsolationScope>,
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

            /// Adds an extra workspace-projection root.
            pub fn add_projection_root(mut self, entry: impl Into<String>) -> Self {
                self.extra_roots.push(entry.into());
                self
            }

            /// Returns the extra projection roots.
            pub fn projection_roots(&self) -> &[String] {
                &self.extra_roots
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

#[cfg(test)]
mod tests {
    use super::*;
    use agentscope_harness::agent::sandbox::ExecResult;
    use std::path::PathBuf;

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
}
