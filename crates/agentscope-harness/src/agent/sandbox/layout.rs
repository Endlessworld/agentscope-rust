//! Workspace layout entry types, mirroring
//! `io.agentscope.harness.agent.sandbox.layout.*`.
//!
//! Each entry describes a single file or directory that should be present in the
//! sandbox workspace at startup. Entries can be materialized from inline content,
//! local host files, or remote sources.
//!
//! When `ephemeral` is `true`, an entry is always re-applied on every sandbox start,
//! even when a snapshot exists; non-ephemeral entries are persisted inside the
//! snapshot and restored with the workspace.

use std::collections::BTreeMap;

use super::workspace_spec::WorkspaceSpec;

/// Base trait-like helper: whether an entry is ephemeral.
///
/// Ephemeral entries are always re-applied on session resume regardless of whether a
/// snapshot exists.
fn default_encoding() -> String {
    "UTF-8".to_string()
}

/// Layout entry that creates a file with inline text content.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FileEntry {
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default)]
    pub content: String,
    #[serde(default = "default_encoding")]
    pub encoding: String,
}

impl Default for FileEntry {
    fn default() -> Self {
        Self { ephemeral: false, content: String::new(), encoding: default_encoding() }
    }
}

impl FileEntry {
    pub fn new(content: impl Into<String>) -> Self {
        Self { content: content.into(), ..Default::default() }
    }

    pub fn with_encoding(content: impl Into<String>, encoding: impl Into<String>) -> Self {
        Self { ephemeral: false, content: content.into(), encoding: encoding.into() }
    }

    pub fn ephemeral(mut self) -> Self {
        self.ephemeral = true;
        self
    }
}

/// Layout entry that creates a directory, optionally with nested child entries.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DirEntry {
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default)]
    pub children: BTreeMap<String, WorkspaceEntry>,
}

impl DirEntry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_children(children: BTreeMap<String, WorkspaceEntry>) -> Self {
        Self { ephemeral: false, children }
    }

    /// Adds a child entry, returning `self` for chaining.
    pub fn child(mut self, name: impl Into<String>, entry: WorkspaceEntry) -> Self {
        self.children.insert(name.into(), entry);
        self
    }
}

/// Layout entry that copies a single file from the host filesystem into the workspace.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LocalFileEntry {
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default)]
    pub source_path: String,
}

impl LocalFileEntry {
    pub fn new(source_path: impl Into<String>) -> Self {
        Self { ephemeral: false, source_path: source_path.into() }
    }
}

/// Layout entry that recursively copies a host directory into the workspace.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LocalDirEntry {
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default)]
    pub source_path: String,
}

impl LocalDirEntry {
    pub fn new(source_path: impl Into<String>) -> Self {
        Self { ephemeral: false, source_path: source_path.into() }
    }
}

/// Layout entry that clones a Git repository into the sandbox workspace.
///
/// The repository is cloned from `url` at the given `ref` (branch, tag, or commit SHA).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GitRepoEntry {
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default)]
    pub url: String,
    #[serde(default = "default_git_ref")]
    pub reference: String,
}

fn default_git_ref() -> String {
    "HEAD".to_string()
}

impl Default for GitRepoEntry {
    fn default() -> Self {
        Self { ephemeral: false, url: String::new(), reference: default_git_ref() }
    }
}

impl GitRepoEntry {
    pub fn new(url: impl Into<String>, reference: impl Into<String>) -> Self {
        Self { ephemeral: false, url: url.into(), reference: reference.into() }
    }
}

/// Layout entry that projects selected host workspace files/dirs into the sandbox.
///
/// This entry is sandbox-specific and is ignored by regular file materialization;
/// projection is applied inside `Sandbox::start()` via archive hydration.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceProjectionEntry {
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default)]
    pub source_root: String,
    #[serde(default)]
    pub include_roots: Vec<String>,
}

/// Layout entry that declares a host directory (or file on Kubernetes) bind-mounted
/// into the sandbox workspace at the map key path under the workspace root.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BindMountEntry {
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default)]
    pub host_path: String,
    #[serde(default)]
    pub read_only: bool,
}

impl BindMountEntry {
    pub fn new(host_path: impl Into<String>) -> Self {
        Self { ephemeral: false, host_path: host_path.into(), read_only: false }
    }

    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
}

/// Discriminated union of all workspace layout entries.
///
/// Serializes with an internal `type` tag using the same names as the Java
/// `@JsonSubTypes` (`file`, `dir`, `local_file`, `local_dir`, `git_repo`,
/// `workspace_projection`, `bind_mount`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceEntry {
    File(FileEntry),
    Dir(DirEntry),
    LocalFile(LocalFileEntry),
    LocalDir(LocalDirEntry),
    GitRepo(GitRepoEntry),
    WorkspaceProjection(WorkspaceProjectionEntry),
    BindMount(BindMountEntry),
}

impl WorkspaceEntry {
    /// Returns whether this entry is ephemeral.
    pub fn is_ephemeral(&self) -> bool {
        match self {
            WorkspaceEntry::File(e) => e.ephemeral,
            WorkspaceEntry::Dir(e) => e.ephemeral,
            WorkspaceEntry::LocalFile(e) => e.ephemeral,
            WorkspaceEntry::LocalDir(e) => e.ephemeral,
            WorkspaceEntry::GitRepo(e) => e.ephemeral,
            WorkspaceEntry::WorkspaceProjection(e) => e.ephemeral,
            WorkspaceEntry::BindMount(e) => e.ephemeral,
        }
    }

    /// Sets the ephemeral flag, returning `self` for chaining.
    pub fn with_ephemeral(mut self, ephemeral: bool) -> Self {
        match &mut self {
            WorkspaceEntry::File(e) => e.ephemeral = ephemeral,
            WorkspaceEntry::Dir(e) => e.ephemeral = ephemeral,
            WorkspaceEntry::LocalFile(e) => e.ephemeral = ephemeral,
            WorkspaceEntry::LocalDir(e) => e.ephemeral = ephemeral,
            WorkspaceEntry::GitRepo(e) => e.ephemeral = ephemeral,
            WorkspaceEntry::WorkspaceProjection(e) => e.ephemeral = ephemeral,
            WorkspaceEntry::BindMount(e) => e.ephemeral = ephemeral,
        }
        self
    }
}

impl From<FileEntry> for WorkspaceEntry {
    fn from(e: FileEntry) -> Self {
        WorkspaceEntry::File(e)
    }
}
impl From<DirEntry> for WorkspaceEntry {
    fn from(e: DirEntry) -> Self {
        WorkspaceEntry::Dir(e)
    }
}
impl From<LocalFileEntry> for WorkspaceEntry {
    fn from(e: LocalFileEntry) -> Self {
        WorkspaceEntry::LocalFile(e)
    }
}
impl From<LocalDirEntry> for WorkspaceEntry {
    fn from(e: LocalDirEntry) -> Self {
        WorkspaceEntry::LocalDir(e)
    }
}
impl From<GitRepoEntry> for WorkspaceEntry {
    fn from(e: GitRepoEntry) -> Self {
        WorkspaceEntry::GitRepo(e)
    }
}
impl From<WorkspaceProjectionEntry> for WorkspaceEntry {
    fn from(e: WorkspaceProjectionEntry) -> Self {
        WorkspaceEntry::WorkspaceProjection(e)
    }
}
impl From<BindMountEntry> for WorkspaceEntry {
    fn from(e: BindMountEntry) -> Self {
        WorkspaceEntry::BindMount(e)
    }
}

/// Describes the desired initial state of a sandbox workspace: root path,
/// materialized file/directory entries, and per-exec environment variables.
///
/// Mirrors the Java `WorkspaceSpec` (entries map). Use [`LayoutSpec::to_workspace_spec`]
/// to bridge to the materialization-oriented [`WorkspaceSpec`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LayoutSpec {
    #[serde(default = "default_root")]
    pub root: String,
    #[serde(default)]
    pub entries: BTreeMap<String, WorkspaceEntry>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

fn default_root() -> String {
    "/workspace".to_string()
}

impl Default for LayoutSpec {
    fn default() -> Self {
        Self { root: default_root(), entries: BTreeMap::new(), environment: BTreeMap::new() }
    }
}

impl LayoutSpec {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_root(mut self, root: impl Into<String>) -> Self {
        self.root = root.into();
        self
    }

    /// Adds an entry by key (workspace-relative path), returning `self` for chaining.
    pub fn entry(mut self, key: impl Into<String>, entry: impl Into<WorkspaceEntry>) -> Self {
        self.entries.insert(key.into(), entry.into());
        self
    }

    pub fn environment(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }

    /// Converts the layout to a materialization-oriented [`WorkspaceSpec`].
    ///
    /// `File` and `Dir` entries are flattened into `directories`/`files`; rich
    /// entries (local copies, git repos, projections, bind mounts) are not
    /// representable and are skipped (they are handled by the sandbox backend).
    pub fn to_workspace_spec(&self) -> WorkspaceSpec {
        let mut spec = WorkspaceSpec::new(self.root.clone());
        spec.environment = self.environment.clone().into_iter().collect();
        for (key, entry) in &self.entries {
            flatten_entry(key, entry, &mut spec);
        }
        spec
    }
}

fn flatten_entry(prefix: &str, entry: &WorkspaceEntry, spec: &mut WorkspaceSpec) {
    match entry {
        WorkspaceEntry::File(f) => {
            spec.files.insert(prefix.to_string(), f.content.clone());
        }
        WorkspaceEntry::Dir(d) => {
            spec.directories.push(prefix.to_string());
            for (name, child) in &d.children {
                let child_path = format!("{}/{}", prefix.trim_end_matches('/'), name);
                flatten_entry(&child_path, child, spec);
            }
        }
        // Non-materializable entries are handled by the sandbox backend at start time.
        WorkspaceEntry::LocalFile(_)
        | WorkspaceEntry::LocalDir(_)
        | WorkspaceEntry::GitRepo(_)
        | WorkspaceEntry::WorkspaceProjection(_)
        | WorkspaceEntry::BindMount(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_entry_defaults_and_builder() {
        let f = FileEntry::new("hello");
        assert_eq!(f.content, "hello");
        assert_eq!(f.encoding, "UTF-8");
        assert!(!f.ephemeral);
        assert!(FileEntry::new("x").ephemeral().ephemeral);
    }

    #[test]
    fn dir_entry_nests_children() {
        let dir = DirEntry::new()
            .child("a.md", FileEntry::new("a").into())
            .child("sub", DirEntry::new().child("b.md", FileEntry::new("b").into()).into());
        assert_eq!(dir.children.len(), 2);
        assert!(matches!(dir.children.get("sub"), Some(WorkspaceEntry::Dir(_))));
    }

    #[test]
    fn entry_serde_uses_type_tag() {
        let entry: WorkspaceEntry = FileEntry::new("x").into();
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "file");
        assert_eq!(json["content"], "x");

        let bind: WorkspaceEntry = BindMountEntry::new("/host").read_only().into();
        let json = serde_json::to_value(&bind).unwrap();
        assert_eq!(json["type"], "bind_mount");
        assert_eq!(json["host_path"], "/host");
        assert_eq!(json["read_only"], true);
    }

    #[test]
    fn entry_roundtrips_from_json() {
        let json = serde_json::json!({
            "type": "git_repo",
            "url": "https://example.com/r.git",
            "reference": "main"
        });
        let entry: WorkspaceEntry = serde_json::from_value(json).unwrap();
        match entry {
            WorkspaceEntry::GitRepo(g) => {
                assert_eq!(g.url, "https://example.com/r.git");
                assert_eq!(g.reference, "main");
            }
            other => panic!("unexpected entry: {other:?}"),
        }
    }

    #[test]
    fn dir_entry_roundtrips_nested_json() {
        let json = serde_json::json!({
            "type": "dir",
            "children": {
                "README.md": {"type": "file", "content": "# hi"},
                "src": {"type": "dir", "children": {}}
            }
        });
        let entry: WorkspaceEntry = serde_json::from_value(json).unwrap();
        match entry {
            WorkspaceEntry::Dir(d) => {
                assert_eq!(d.children.len(), 2);
                assert!(matches!(d.children.get("README.md"), Some(WorkspaceEntry::File(_))));
            }
            other => panic!("unexpected entry: {other:?}"),
        }
    }

    #[test]
    fn layout_spec_flattens_to_workspace_spec() {
        let layout = LayoutSpec::new()
            .with_root("/ws")
            .entry("README.md", FileEntry::new("# Hi"))
            .entry(
                "src",
                DirEntry::new().child("main.rs", FileEntry::new("fn main() {}").into()),
            )
            .environment("DEBUG", "true");
        let spec = layout.to_workspace_spec();
        assert_eq!(spec.root, std::path::PathBuf::from("/ws"));
        assert_eq!(spec.files.get("README.md").unwrap(), "# Hi");
        assert_eq!(spec.files.get("src/main.rs").unwrap(), "fn main() {}");
        assert!(spec.directories.contains(&"src".to_string()));
        assert_eq!(spec.environment.get("DEBUG").unwrap(), "true");
    }

    #[test]
    fn ephemeral_accessor_and_setter() {
        let entry: WorkspaceEntry = FileEntry::new("x").into();
        assert!(!entry.is_ephemeral());
        assert!(entry.with_ephemeral(true).is_ephemeral());
    }
}
