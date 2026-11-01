//! Workspace specification types, mirroring `WorkspaceSpec`, `WorkspaceSpecApplier`,
//! `DefaultWorkspaceSpecApplier`, and `ExecResult`.
//!
//! These types describe the desired layout of a sandbox workspace and provide
//! the mechanism to apply that layout when the sandbox is started.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use agentscope_core::error::Result;

use super::layout::{BindMountEntry, WorkspaceProjectionEntry};

/// Describes the desired layout of a sandbox workspace.
///
/// A workspace spec contains the list of directories, files, and environment
/// configuration that should be present when the sandbox starts.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceSpec {
    /// Root directory for the workspace (typically `/workspace` inside a sandbox).
    pub root: PathBuf,

    /// Directories to create within the workspace.
    pub directories: Vec<String>,

    /// Files to create with their content (relative path → content).
    pub files: HashMap<String, String>,

    /// Working directory relative to the workspace root.
    pub working_directory: Option<String>,

    /// Environment variables to set.
    pub environment: HashMap<String, String>,

    /// Paths to copy from the host into the workspace.
    pub copy_in: Vec<CopySpec>,

    /// Paths to export from the workspace after execution.
    pub copy_out: Vec<String>,

    /// Host directories bind-mounted into the workspace, keyed by their mount path under
    /// `root` (Java `WorkspaceSpec.entries` with `BindMountEntry` values).
    #[serde(default)]
    pub bind_mounts: BTreeMap<String, BindMountEntry>,

    /// Host workspace projected into the sandbox at start (Java
    /// `WorkspaceSpec.__workspace_projection__`).
    #[serde(default)]
    pub workspace_projection: Option<WorkspaceProjectionEntry>,
}

impl WorkspaceSpec {
    /// Creates a new workspace spec with the given root path.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            ..Default::default()
        }
    }

    /// Sets the working directory.
    pub fn with_working_directory(mut self, dir: impl Into<String>) -> Self {
        self.working_directory = Some(dir.into());
        self
    }

    /// Adds a directory to create.
    pub fn with_directory(mut self, dir: impl Into<String>) -> Self {
        self.directories.push(dir.into());
        self
    }

    /// Adds a file with content.
    pub fn with_file(mut self, path: impl Into<String>, content: impl Into<String>) -> Self {
        self.files.insert(path.into(), content.into());
        self
    }

    /// Adds an environment variable.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }

    /// Adds a copy-in specification.
    pub fn with_copy_in(mut self, spec: CopySpec) -> Self {
        self.copy_in.push(spec);
        self
    }

    /// Adds a path to copy out after execution.
    pub fn with_copy_out(mut self, path: impl Into<String>) -> Self {
        self.copy_out.push(path.into());
        self
    }

    /// Bind-mounts `host_path` at `path` (workspace-relative).
    pub fn with_bind_mount(
        mut self,
        path: impl Into<String>,
        host_path: impl Into<String>,
    ) -> Self {
        self.bind_mounts
            .insert(path.into(), BindMountEntry::new(host_path));
        self
    }

    /// Resolves the full path for a workspace-relative path.
    pub fn resolve(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Returns the effective working directory as a workspace-relative path.
    pub fn effective_cwd(&self) -> &str {
        self.working_directory.as_deref().unwrap_or(".")
    }
}

/// Specification for copying a file or directory into the sandbox workspace.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CopySpec {
    /// Source path (host filesystem or remote URI).
    pub source: String,

    /// Destination path within the workspace (relative to workspace root).
    pub destination: String,

    /// Whether the copy is read-only.
    pub read_only: bool,
}

impl CopySpec {
    pub fn new(source: impl Into<String>, destination: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            destination: destination.into(),
            read_only: false,
        }
    }

    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
}

/// Result of executing a command within a sandbox.
#[derive(Debug, Clone)]
pub struct ExecResult {
    /// Combined stdout output.
    pub stdout: String,

    /// Combined stderr output.
    pub stderr: String,

    /// The exit code of the command.
    pub exit_code: i32,

    /// Whether the output was truncated due to size limits.
    pub truncated: bool,
}

impl ExecResult {
    /// Returns `true` if the command exited successfully (exit code 0).
    pub fn is_success(&self) -> bool {
        self.exit_code == 0
    }

    /// Returns `true` if the command timed out (conventionally exit code -1).
    pub fn is_timeout(&self) -> bool {
        self.exit_code == -1
    }

    /// Returns the combined output (stdout + stderr).
    pub fn combined_output(&self) -> String {
        let mut out = self.stdout.clone();
        if !self.stderr.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&self.stderr);
        }
        out
    }
}

/// Trait for applying a [`WorkspaceSpec`] to create or prepare a workspace.
#[async_trait::async_trait]
pub trait WorkspaceSpecApplier: Send + Sync {
    /// Applies the workspace spec, creating directories, files, and copying in assets.
    async fn apply(&self, spec: &WorkspaceSpec) -> Result<()>;

    /// Cleans up resources created by the spec application.
    async fn cleanup(&self, spec: &WorkspaceSpec) -> Result<()>;
}

/// Default implementation of `WorkspaceSpecApplier` that creates directories
/// and writes files using the local filesystem.
pub struct DefaultWorkspaceSpecApplier;

#[async_trait::async_trait]
impl WorkspaceSpecApplier for DefaultWorkspaceSpecApplier {
    async fn apply(&self, spec: &WorkspaceSpec) -> Result<()> {
        // Create the workspace root
        tokio::fs::create_dir_all(&spec.root).await?;

        // Create directories
        for dir in &spec.directories {
            let path = spec.root.join(dir);
            tokio::fs::create_dir_all(&path).await?;
        }

        // Write files
        for (rel_path, content) in &spec.files {
            let path = spec.root.join(rel_path);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&path, content).await?;
        }

        // Create working directory if specified
        if let Some(cwd) = &spec.working_directory {
            let path = spec.root.join(cwd);
            tokio::fs::create_dir_all(&path).await?;
        }

        tracing::debug!("Applied workspace spec to {:?}", spec.root);
        Ok(())
    }

    async fn cleanup(&self, spec: &WorkspaceSpec) -> Result<()> {
        if spec.root.exists() {
            tokio::fs::remove_dir_all(&spec.root).await?;
            tracing::debug!("Cleaned up workspace at {:?}", spec.root);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workspace_spec_builder() {
        let spec = WorkspaceSpec::new("/workspace")
            .with_directory("src")
            .with_directory("tests")
            .with_file("README.md", "# Hello")
            .with_env("RUST_LOG", "debug")
            .with_working_directory("src");

        assert_eq!(spec.root, PathBuf::from("/workspace"));
        assert_eq!(spec.directories, vec!["src", "tests"]);
        assert_eq!(spec.files.get("README.md").unwrap(), "# Hello");
        assert_eq!(spec.environment.get("RUST_LOG").unwrap(), "debug");
        assert_eq!(spec.effective_cwd(), "src");
    }

    #[test]
    fn test_exec_result() {
        let result = ExecResult {
            stdout: "hello".into(),
            stderr: "err".into(),
            exit_code: 0,
            truncated: false,
        };
        assert!(result.is_success());
        assert!(!result.is_timeout());
        assert_eq!(result.combined_output(), "hello\nerr");
    }

    #[test]
    fn test_exec_result_timeout() {
        let result = ExecResult {
            stdout: String::new(),
            stderr: "timed out".into(),
            exit_code: -1,
            truncated: true,
        };
        assert!(!result.is_success());
        assert!(result.is_timeout());
    }

    #[test]
    fn test_copy_spec() {
        let spec = CopySpec::new("/host/data", "data").read_only();
        assert!(spec.read_only);
    }

    #[test]
    fn test_workspace_spec_resolve() {
        let spec = WorkspaceSpec::new("/workspace");
        assert_eq!(
            spec.resolve("src/main.rs"),
            PathBuf::from("/workspace/src/main.rs")
        );
    }
}
