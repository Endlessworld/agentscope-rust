//! Declarative filesystem specifications, mirroring
//! `io.agentscope.harness.agent.filesystem.spec.*`.
//!
//! A spec describes *how* to assemble a `Filesystem` from a workspace root, so the caller
//! can defer assembly to build time instead of hand-wiring the same stack everywhere:
//!
//! - [`LocalFilesystemSpec`] — local disk, optionally copy-on-write over a project tree;
//! - [`RemoteFilesystemSpec`] — `CompositeFilesystem` with per-route shared-store backends;
//! - [`SandboxFilesystemSpec`] — a `SandboxContext` (not a filesystem: the sandbox filesystem
//!   only exists once a sandbox is acquired, then wrapped by
//!   [`PinnedSandboxFilesystem`](super::PinnedSandboxFilesystem)).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentscope_core::agent::RuntimeContext;

use super::{
    BaseStore, CompositeFilesystem, Filesystem, LocalFilesystem, LocalFsMode, NamespaceFactory,
    OverlayFilesystem, RemoteFilesystem,
};
use crate::agent::sandbox::{
    LayoutSpec, SandboxClient, SandboxClientOptions, SandboxContext, SandboxSnapshotSpec,
    WorkspaceProjectionEntry, WorkspaceSpec,
};
use crate::agent::IsolationScope;

/// Exact workspace-root files routed to the shared store (Java `RemoteFilesystemSpec`).
const ROOT_SHARED_FILES: [&str; 3] = ["AGENTS.md", "MEMORY.md", "tools.json"];

/// Workspace prefixes routed to the shared store (Java `RemoteFilesystemSpec`).
const SHARED_PREFIXES: [&str; 5] = ["memory/", "skills/", "subagents/", "knowledge/", "plans/"];

/// Workspace roots projected into the sandbox workspace by default (Java
/// `SandboxFilesystemSpec.DEFAULT_WORKSPACE_PROJECTION_ROOTS`).
pub const DEFAULT_WORKSPACE_PROJECTION_ROOTS: [&str; 5] = [
    "AGENTS.md",
    "skills",
    "subagents",
    "knowledge",
    ".skills-cache",
];

// ---------------------------------------------------------------------------
// LocalFilesystemSpec
// ---------------------------------------------------------------------------

/// Specification for the local-disk filesystem mode.
///
/// The agent workspace is the read-write layer; `project` (when set) is a read-only lower
/// layer, so project-authored `AGENTS.md` / `knowledge/` stay visible while edits copy up
/// into the workspace (Java `LocalFilesystemSpec` + `OverlayFilesystem.of(upper, lower)`).
#[derive(Debug, Clone)]
pub struct LocalFilesystemSpec {
    mode: LocalFsMode,
    max_file_size_mb: usize,
    additional_roots: Vec<PathBuf>,
    project: Option<PathBuf>,
}

impl Default for LocalFilesystemSpec {
    fn default() -> Self {
        Self {
            // Java defaults to `LocalFsMode.ROOTED`: absolute agent paths are accepted only
            // under the configured roots.
            mode: LocalFsMode::Rooted,
            max_file_size_mb: 10,
            additional_roots: Vec::new(),
            project: None,
        }
    }
}

impl LocalFilesystemSpec {
    pub fn new() -> Self {
        Self::default()
    }

    /// Path-resolution policy for the workspace layer.
    pub fn mode(mut self, mode: LocalFsMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn max_file_size_mb(mut self, megabytes: usize) -> Self {
        self.max_file_size_mb = megabytes;
        self
    }

    /// Adds an extra host directory accepted by absolute path in [`LocalFsMode::Rooted`]
    /// (Java `addRoot` / `additionalRoots`).
    pub fn add_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.additional_roots.push(root.into());
        self
    }

    /// Replaces the extra roots.
    pub fn additional_roots(mut self, roots: impl IntoIterator<Item = PathBuf>) -> Self {
        self.additional_roots = roots.into_iter().collect();
        self
    }

    /// Read-only project tree used as the overlay's lower layer.
    pub fn project(mut self, project: impl Into<PathBuf>) -> Self {
        self.project = Some(project.into());
        self
    }

    pub fn project_root(&self) -> Option<&Path> {
        self.project.as_deref()
    }

    pub fn get_mode(&self) -> LocalFsMode {
        self.mode
    }

    /// Builds the filesystem: a plain `LocalFilesystem` when no project is configured,
    /// otherwise the workspace-over-project copy-on-write overlay.
    pub fn build(&self, workspace: &Path) -> Arc<dyn Filesystem> {
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(project) = &self.project {
            roots.push(project.clone());
        }
        roots.push(workspace.to_path_buf());
        roots.extend(self.additional_roots.iter().cloned());

        let upper = LocalFilesystem::with_mode(workspace, self.mode, self.max_file_size_mb)
            .with_additional_roots(roots);
        let Some(project) = &self.project else {
            return Arc::new(upper);
        };
        let lower = LocalFilesystem::sandboxed(project);
        Arc::new(OverlayFilesystem::new(
            Arc::new(upper) as Arc<dyn Filesystem>,
            Arc::new(lower) as Arc<dyn Filesystem>,
        ))
    }
}

// ---------------------------------------------------------------------------
// RemoteFilesystemSpec
// ---------------------------------------------------------------------------

/// Specification for the composite (non-sandbox) filesystem mode.
///
/// Shared workspace paths (memory, skills, knowledge, sessions, …) are routed to a
/// [`BaseStore`] so they survive across replicas; everything else stays on local disk.
/// Shell execution is not available in this mode (Java `RemoteFilesystemSpec`).
#[derive(Clone)]
pub struct RemoteFilesystemSpec {
    store: Arc<dyn BaseStore>,
    isolation_scope: IsolationScope,
    anonymous_user_id: String,
    extra_shared_prefixes: Vec<String>,
    shared_local_workspace: bool,
}

impl RemoteFilesystemSpec {
    pub fn new(store: Arc<dyn BaseStore>) -> Self {
        Self {
            store,
            isolation_scope: IsolationScope::User,
            anonymous_user_id: "_default".to_string(),
            extra_shared_prefixes: Vec::new(),
            shared_local_workspace: false,
        }
    }

    /// Store namespace granularity for shared files (default [`IsolationScope::User`]).
    pub fn isolation_scope(mut self, scope: IsolationScope) -> Self {
        self.isolation_scope = scope;
        self
    }

    pub fn get_isolation_scope(&self) -> IsolationScope {
        self.isolation_scope
    }

    /// Fallback user identifier when the runtime context carries none.
    pub fn anonymous_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.anonymous_user_id = user_id.into();
        self
    }

    /// Routes an extra workspace-relative prefix to the shared store.
    pub fn add_shared_prefix(mut self, prefix: impl Into<String>) -> Self {
        let normalized = normalize_prefix(&prefix.into());
        if !normalized.is_empty() {
            self.extra_shared_prefixes.push(normalized);
        }
        self
    }

    /// When `true`, the default backend serves the workspace root directly (sandboxed paths,
    /// no per-user layout) instead of the unrestricted local backend.
    pub fn shared_local_workspace(mut self, shared: bool) -> Self {
        self.shared_local_workspace = shared;
        self
    }

    /// Builds the composite filesystem: local default backend + per-route shared stores.
    pub fn build(&self, workspace: &Path, agent_id: &str) -> Arc<dyn Filesystem> {
        let agent_id = if agent_id.trim().is_empty() {
            "HarnessAgent"
        } else {
            agent_id
        };
        let default_backend: Arc<dyn Filesystem> = if self.shared_local_workspace {
            Arc::new(LocalFilesystem::sandboxed(workspace))
        } else {
            Arc::new(LocalFilesystem::new(workspace))
        };
        let workspace_template = || -> Arc<dyn Filesystem> {
            Arc::new(LocalFilesystem::sandboxed(workspace)) as Arc<dyn Filesystem>
        };

        let mut routes: Vec<(String, Arc<dyn Filesystem>)> = Vec::new();
        // Exact workspace-root files: shared-store upper over the workspace template lower,
        // so a scaffolded `AGENTS.md` is visible until the agent edits it.
        for file in ROOT_SHARED_FILES {
            routes.push((
                file.to_string(),
                self.overlay_route("root", None, agent_id, workspace_template()),
            ));
        }
        for prefix in SHARED_PREFIXES {
            routes.push(self.prefix_route(workspace, prefix, route_segment(prefix), agent_id));
        }
        let sessions = format!("agents/{agent_id}/sessions/");
        routes.push(self.prefix_route(workspace, &sessions, "sessions", agent_id));
        let tasks = format!("agents/{agent_id}/tasks/");
        routes.push(self.prefix_route(workspace, &tasks, "tasks", agent_id));
        for prefix in &self.extra_shared_prefixes {
            routes.push(self.prefix_route(workspace, prefix, route_segment(prefix), agent_id));
        }
        Arc::new(CompositeFilesystem::new(default_backend, routes))
    }

    /// One workspace-prefix route: shared store upper over the local template directory.
    fn prefix_route(
        &self,
        workspace: &Path,
        prefix: &str,
        namespace_segment: &str,
        agent_id: &str,
    ) -> (String, Arc<dyn Filesystem>) {
        let template = workspace.join(prefix.trim_matches('/'));
        let lower = Arc::new(LocalFilesystem::sandboxed(template)) as Arc<dyn Filesystem>;
        (
            prefix.to_string(),
            self.overlay_route(prefix, Some(namespace_segment), agent_id, lower),
        )
    }

    fn overlay_route(
        &self,
        route_segment: &str,
        namespace_segment: Option<&str>,
        agent_id: &str,
        lower: Arc<dyn Filesystem>,
    ) -> Arc<dyn Filesystem> {
        let namespace: Arc<dyn NamespaceFactory> = Arc::new(ScopedRouteNamespace {
            scope: self.isolation_scope,
            agent_id: agent_id.to_string(),
            anonymous_user_id: self.anonymous_user_id.clone(),
            segment: namespace_segment
                .map(str::to_string)
                .unwrap_or_else(|| route_segment.to_string()),
        });
        let upper = Arc::new(RemoteFilesystem::new(Arc::clone(&self.store), namespace))
            as Arc<dyn Filesystem>;
        Arc::new(OverlayFilesystem::new(upper, lower))
    }
}

/// Per-route store namespace, mirroring the Java `storeNamespace` switch.
struct ScopedRouteNamespace {
    scope: IsolationScope,
    agent_id: String,
    anonymous_user_id: String,
    segment: String,
}

impl NamespaceFactory for ScopedRouteNamespace {
    fn get_namespace(&self, runtime_context: &RuntimeContext) -> Vec<String> {
        let mut namespace = match self.scope {
            IsolationScope::Session => {
                let session = runtime_context
                    .session_id()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or("default");
                vec![
                    "agents".to_string(),
                    self.agent_id.clone(),
                    "sessions".to_string(),
                    session.to_string(),
                ]
            }
            IsolationScope::User => {
                let user = runtime_context
                    .user_id()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(&self.anonymous_user_id);
                vec![
                    "agents".to_string(),
                    self.agent_id.clone(),
                    "users".to_string(),
                    user.to_string(),
                ]
            }
            IsolationScope::Agent => vec![
                "agents".to_string(),
                self.agent_id.clone(),
                "shared".to_string(),
            ],
            IsolationScope::Global => vec!["global".to_string()],
        };
        namespace.push(self.segment.clone());
        namespace
    }
}

/// Normalizes a configured prefix to a workspace-relative, slash-separated route.
fn normalize_prefix(prefix: &str) -> String {
    prefix
        .replace('\\', "/")
        .trim()
        .trim_matches('/')
        .to_string()
}

/// Namespace segment for a workspace prefix (`memory/` → `memory`).
fn route_segment(prefix: &str) -> &str {
    prefix.trim_matches('/')
}

// ---------------------------------------------------------------------------
// SandboxFilesystemSpec
// ---------------------------------------------------------------------------

/// Declarative sandbox filesystem configuration (Java `SandboxFilesystemSpec`).
///
/// Not a runtime filesystem: it assembles the [`SandboxContext`] a sandbox-backed agent is
/// created from, including the workspace layout, bind mounts and the workspace projection.
/// The resulting sandbox filesystem is wrapped by
/// [`PinnedSandboxFilesystem`](super::PinnedSandboxFilesystem) once acquired.
pub struct SandboxFilesystemSpec {
    client: Box<dyn SandboxClient>,
    client_options: Option<SandboxClientOptions>,
    snapshot_spec: Option<Arc<dyn SandboxSnapshotSpec>>,
    layout: LayoutSpec,
    isolation_scope: Option<IsolationScope>,
    workspace_projection_enabled: bool,
    workspace_projection_roots: Vec<String>,
}

impl std::fmt::Debug for SandboxFilesystemSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxFilesystemSpec")
            .field("client_options", &self.client_options)
            .field("layout", &self.layout)
            .field("isolation_scope", &self.isolation_scope)
            .field(
                "workspace_projection_enabled",
                &self.workspace_projection_enabled,
            )
            .field(
                "workspace_projection_roots",
                &self.workspace_projection_roots,
            )
            .finish()
    }
}

impl SandboxFilesystemSpec {
    pub fn new(client: Box<dyn SandboxClient>) -> Self {
        Self {
            client,
            client_options: None,
            snapshot_spec: None,
            layout: LayoutSpec::new(),
            isolation_scope: None,
            workspace_projection_enabled: true,
            workspace_projection_roots: DEFAULT_WORKSPACE_PROJECTION_ROOTS
                .iter()
                .map(|root| root.to_string())
                .collect(),
        }
    }

    /// Desired initial workspace layout (files, directories, bind mounts, projection).
    pub fn layout(mut self, layout: LayoutSpec) -> Self {
        self.layout = layout;
        self
    }

    pub fn layout_spec(&self) -> &LayoutSpec {
        &self.layout
    }

    pub fn client_options(mut self, options: SandboxClientOptions) -> Self {
        self.client_options = Some(options);
        self
    }

    pub fn snapshot_spec(mut self, snapshot_spec: Arc<dyn SandboxSnapshotSpec>) -> Self {
        self.snapshot_spec = Some(snapshot_spec);
        self
    }

    pub fn isolation_scope(mut self, scope: IsolationScope) -> Self {
        self.isolation_scope = Some(scope);
        self
    }

    pub fn get_isolation_scope(&self) -> Option<IsolationScope> {
        self.isolation_scope
    }

    pub fn workspace_projection_enabled(mut self, enabled: bool) -> Self {
        self.workspace_projection_enabled = enabled;
        self
    }

    /// Replaces the workspace roots projected into the sandbox (default
    /// [`DEFAULT_WORKSPACE_PROJECTION_ROOTS`]).
    pub fn workspace_projection_roots(
        mut self,
        roots: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.workspace_projection_roots = roots.into_iter().map(Into::into).collect();
        self
    }

    /// The effective workspace spec: layout entries flattened, bind mounts carried through and
    /// the workspace projection attached when a host workspace is known.
    pub fn workspace_spec(&self, host_workspace_root: Option<&Path>) -> WorkspaceSpec {
        let mut spec = self.layout.to_workspace_spec();
        if !self.workspace_projection_enabled {
            return spec;
        }
        let Some(root) = host_workspace_root else {
            return spec;
        };
        spec.workspace_projection = Some(WorkspaceProjectionEntry {
            ephemeral: false,
            source_root: root.to_string_lossy().replace('\\', "/"),
            include_roots: self.workspace_projection_roots.clone(),
        });
        spec
    }

    /// Consumes the spec into a [`SandboxContext`] (Java `toSandboxContext`).
    pub fn into_sandbox_context(self, host_workspace_root: Option<&Path>) -> SandboxContext {
        let workspace_spec = self.workspace_spec(host_workspace_root);
        let mut builder = SandboxContext::builder()
            .client(self.client)
            .workspace_spec(workspace_spec);
        if let Some(options) = self.client_options {
            builder = builder.client_options(options);
        }
        if let Some(snapshot_spec) = self.snapshot_spec {
            builder = builder.snapshot_spec(snapshot_spec);
        }
        if let Some(scope) = self.isolation_scope {
            builder = builder.isolation_scope(scope);
        }
        builder.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::sandbox::{BindMountEntry, DirEntry, Sandbox, SandboxState, WorkspaceSpec};
    use crate::filesystem::{FilesystemKind, InMemoryStore};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fs_spec_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn local_spec_builds_a_copy_on_write_overlay_over_the_project() {
        let root = temp_dir("local");
        let project = root.join("project");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(project.join("AGENTS.md"), "project rules").unwrap();

        let fs = LocalFilesystemSpec::new()
            .project(&project)
            .build(&workspace);
        assert_eq!(fs.kind(), FilesystemKind::Overlay);

        let ctx = RuntimeContext::default();
        // Reads fall back to the project tree …
        assert_eq!(
            fs.read(&ctx, "AGENTS.md", 0, 100).await.unwrap().content,
            "project rules"
        );
        // … while writes copy up into the workspace and leave the project untouched.
        fs.write(&ctx, "AGENTS.md", "agent rules").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("AGENTS.md")).unwrap(),
            "agent rules"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("AGENTS.md")).unwrap(),
            "project rules"
        );
        assert_eq!(
            fs.read(&ctx, "AGENTS.md", 0, 100).await.unwrap().content,
            "agent rules"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn local_spec_without_a_project_stays_a_plain_local_filesystem() {
        let workspace = temp_dir("local_plain");
        let fs = LocalFilesystemSpec::new().build(&workspace);
        assert_eq!(fs.kind(), FilesystemKind::Local);
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn remote_spec_routes_shared_prefixes_to_the_store() {
        let root = temp_dir("remote");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        std::fs::write(workspace.join("memory").join("notes.md"), "template").unwrap();

        let store = Arc::new(InMemoryStore::new());
        let fs = RemoteFilesystemSpec::new(store.clone()).build(&workspace, "agent-x");
        assert_eq!(fs.kind(), FilesystemKind::Composite);

        let ctx = RuntimeContext::default().with_user_id("u1");
        // The scaffolded template is the baseline until the agent writes.
        assert_eq!(
            fs.read(&ctx, "memory/notes.md", 0, 100)
                .await
                .unwrap()
                .content,
            "template"
        );
        fs.write(&ctx, "memory/notes.md", "agent note")
            .await
            .unwrap();
        assert_eq!(store.size(), 1, "the edit must land in the shared store");
        assert_eq!(
            fs.read(&ctx, "memory/notes.md", 0, 100)
                .await
                .unwrap()
                .content,
            "agent note"
        );

        // Everything outside the shared routes stays on local disk.
        fs.write(&ctx, "uploads/a.md", "local").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("uploads").join("a.md")).unwrap(),
            "local"
        );
        assert_eq!(store.size(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remote_spec_scopes_the_store_namespace_by_isolation() {
        let users = ScopedRouteNamespace {
            scope: IsolationScope::User,
            agent_id: "agent-x".to_string(),
            anonymous_user_id: "_default".to_string(),
            segment: "memory".to_string(),
        };
        let ctx = RuntimeContext::default().with_user_id("u1");
        assert_eq!(
            users.get_namespace(&ctx),
            vec!["agents", "agent-x", "users", "u1", "memory"]
        );
        // No user in the context falls back to the configured anonymous id.
        assert_eq!(
            users.get_namespace(&RuntimeContext::default()),
            vec!["agents", "agent-x", "users", "_default", "memory"]
        );

        let sessions = ScopedRouteNamespace {
            scope: IsolationScope::Session,
            agent_id: "agent-x".to_string(),
            anonymous_user_id: "_default".to_string(),
            segment: "tasks".to_string(),
        };
        assert_eq!(
            sessions.get_namespace(&RuntimeContext::default()),
            vec!["agents", "agent-x", "sessions", "default", "tasks"]
        );

        let global = ScopedRouteNamespace {
            scope: IsolationScope::Global,
            agent_id: "agent-x".to_string(),
            anonymous_user_id: "_default".to_string(),
            segment: "skills".to_string(),
        };
        assert_eq!(
            global.get_namespace(&RuntimeContext::default()),
            vec!["global", "skills"]
        );
    }

    struct NoopSandboxClient;

    #[async_trait::async_trait]
    impl SandboxClient for NoopSandboxClient {
        async fn create(
            &self,
            _workspace_spec: &WorkspaceSpec,
            _snapshot_spec: Option<&dyn SandboxSnapshotSpec>,
            _options: Option<&SandboxClientOptions>,
        ) -> agentscope_core::error::Result<Box<dyn Sandbox>> {
            Err(agentscope_core::Error::Tool("unused test client".into()))
        }
        async fn resume(
            &self,
            _state: &SandboxState,
        ) -> agentscope_core::error::Result<Box<dyn Sandbox>> {
            Err(agentscope_core::Error::Tool("unused test client".into()))
        }
        async fn delete(&self, _sandbox: &dyn Sandbox) -> agentscope_core::error::Result<()> {
            Ok(())
        }
        fn serialize_state(&self, _state: &SandboxState) -> agentscope_core::error::Result<String> {
            Err(agentscope_core::Error::Tool("unused test client".into()))
        }
        fn deserialize_state(&self, _json: &str) -> agentscope_core::error::Result<SandboxState> {
            Err(agentscope_core::Error::Tool("unused test client".into()))
        }
    }

    #[test]
    fn sandbox_spec_carries_bind_mounts_and_projection_into_the_workspace_spec() {
        let layout = LayoutSpec::new()
            .with_root("/workspace")
            .entry("memory", BindMountEntry::new("/host/memory"))
            .entry("skills", DirEntry::new());

        let spec = SandboxFilesystemSpec::new(Box::new(NoopSandboxClient)).layout(layout);
        let workspace_spec = spec.workspace_spec(Some(Path::new("/host/workspace")));

        assert_eq!(workspace_spec.root, PathBuf::from("/workspace"));
        assert!(workspace_spec.directories.contains(&"skills".to_string()));
        let mount = workspace_spec
            .bind_mounts
            .get("memory")
            .expect("bind mount carried through");
        assert_eq!(mount.host_path, "/host/memory");
        let projection = workspace_spec
            .workspace_projection
            .expect("workspace projection attached");
        assert_eq!(projection.source_root, "/host/workspace");
        assert_eq!(
            projection.include_roots,
            DEFAULT_WORKSPACE_PROJECTION_ROOTS
                .iter()
                .map(|root| root.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn sandbox_spec_skips_the_projection_when_disabled_or_rootless() {
        let disabled = SandboxFilesystemSpec::new(Box::new(NoopSandboxClient))
            .workspace_projection_enabled(false)
            .workspace_spec(Some(Path::new("/host/workspace")));
        assert!(disabled.workspace_projection.is_none());

        let rootless = SandboxFilesystemSpec::new(Box::new(NoopSandboxClient)).workspace_spec(None);
        assert!(rootless.workspace_projection.is_none());
    }

    #[test]
    fn sandbox_spec_builds_a_sandbox_context_with_its_scope_and_workspace_spec() {
        let spec = SandboxFilesystemSpec::new(Box::new(NoopSandboxClient))
            .isolation_scope(IsolationScope::Session)
            .client_options(SandboxClientOptions {
                docker_image: Some("alpine".to_string()),
                ..Default::default()
            });
        let context = spec.into_sandbox_context(Some(Path::new("/host/workspace")));
        assert_eq!(context.isolation_scope(), Some(&IsolationScope::Session));
        assert_eq!(
            context
                .client_options()
                .and_then(|options| options.docker_image.as_deref()),
            Some("alpine")
        );
        assert!(context.client().is_some());
        assert!(context.workspace_spec().is_some());
    }
}
