//! Filesystem abstraction for agents, mirroring `io.agentscope.harness.agent.filesystem`.
//! Provides list/read/write/edit/grep/glob/delete/move operations over a unified
//! `Filesystem` trait, with local, overlay, composite and (stubbed) remote/sandbox backends.

pub mod baked;
pub mod composite;
pub mod local;
pub mod overlay;
pub mod remote;
pub mod result;
pub mod sandbox;
pub mod util;

use std::any::Any;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::Result;
use async_trait::async_trait;

pub use baked::BakedContextFilesystem;
pub use composite::CompositeFilesystem;
pub use local::{LocalFilesystem, LocalFsMode};
pub use overlay::OverlayFilesystem;
pub use remote::store::{
    BaseStore, DefaultNamespace, InMemoryStore, NamespaceFactory, StaticNamespace, StoreItem,
    StoreValue,
};
pub use remote::RemoteFilesystem;
pub use result::{
    EditResult, FileDownloadResponse, FileUploadResponse, GlobResult, GrepResult, LsResult,
    ReadResult, WriteResult,
};
pub use sandbox::{SandboxBackedFilesystem, SandboxFilesystem};

/// Directory/listing entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub path: String,
    pub is_directory: bool,
    pub size: u64,
    pub modified_at: String,
}
impl FileInfo {
    pub fn file(path: impl Into<String>, size: u64) -> Self {
        Self {
            path: path.into(),
            is_directory: false,
            size,
            modified_at: String::new(),
        }
    }
    pub fn dir(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            is_directory: true,
            size: 0,
            modified_at: String::new(),
        }
    }
}

/// File content plus encoding metadata (`utf-8` for text, `base64` for binary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileData {
    pub content: String,
    pub encoding: String,
}

impl FileData {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            encoding: "utf-8".into(),
        }
    }
    pub fn binary(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            encoding: "base64".into(),
        }
    }
}

/// A single grep match (1-indexed line number).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

/// Result of shell/code execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteResponse {
    pub output: String,
    pub exit_code: i32,
    pub truncated: bool,
}

impl ExecuteResponse {
    pub fn is_success(&self) -> bool {
        self.exit_code == 0
    }
}

/// Concrete filesystem family, used where Java would use `instanceof`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemKind {
    Local,
    Overlay,
    Sandbox,
    Composite,
    Remote,
    Other,
}

/// Unified filesystem API for agents.
///
/// Mirrors `AbstractFilesystem`. Every operation accepts a `RuntimeContext` so stores can
/// scope work to the current session/user/sandbox; local backends ignore it.
#[async_trait]
pub trait Filesystem: Send + Sync + 'static {
    /// Downcast hook for the few prompt/configuration paths that need concrete
    /// filesystem metadata (Java uses `instanceof`).
    fn as_any(&self) -> &dyn Any;

    /// Concrete family marker.
    fn kind(&self) -> FilesystemKind {
        FilesystemKind::Other
    }

    async fn ls(&self, ctx: &RuntimeContext, path: &str) -> Result<Vec<FileInfo>>;
    async fn read(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<FileData>;
    async fn write(&self, ctx: &RuntimeContext, path: &str, content: &str) -> Result<()>;
    async fn edit(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<usize>;
    async fn grep(
        &self,
        ctx: &RuntimeContext,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>>;
    async fn glob(&self, ctx: &RuntimeContext, pattern: &str, path: &str) -> Result<Vec<FileInfo>>;
    async fn delete(&self, ctx: &RuntimeContext, path: &str) -> Result<()>;
    async fn move_path(&self, ctx: &RuntimeContext, from: &str, to: &str) -> Result<()>;
    async fn exists(&self, ctx: &RuntimeContext, path: &str) -> Result<bool>;

    /// Flushes `path` to stable storage.
    ///
    /// Session journal writes use this so a committed head survives a power loss, not just a
    /// process crash. Backends without a durable layer (in-memory, remote stores) keep the
    /// no-op default.
    async fn sync(&self, _ctx: &RuntimeContext, _path: &str) -> Result<()> {
        Ok(())
    }
}

/// Validates that `path` is safe (non-blank, no `..` traversal).
pub fn validate_path(path: &str) -> Result<()> {
    if path.trim().is_empty() {
        return Err(agentscope_core::Error::Tool(
            "Path must not be blank".into(),
        ));
    }
    for seg in path.replace('\\', "/").split('/') {
        if seg == ".." {
            return Err(agentscope_core::Error::Tool(format!(
                "Path traversal ('..') not allowed: {path}"
            )));
        }
    }
    Ok(())
}
