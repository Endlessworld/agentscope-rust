//! Sandbox-aware filesystem: adds shell execution on top of the base `Filesystem` API,
//! mirroring `AbstractSandboxFilesystem`. The default backend runs commands on the local host.
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result};

use super::{ExecuteResponse, FileData, FileInfo, Filesystem, GrepMatch};

/// A `Filesystem` that additionally supports shell command execution.
#[async_trait]
pub trait SandboxFilesystem: Filesystem {
    /// Executes `command` with the given timeout (seconds), returning combined output + exit code.
    async fn execute(
        &self,
        ctx: &RuntimeContext,
        command: &str,
        timeout_seconds: u32,
    ) -> Result<ExecuteResponse>;
}
/// A sandbox filesystem that executes shell commands on the local host, delegating all
/// filesystem operations to an inner `Filesystem`.
pub struct SandboxBackedFilesystem {
    inner: Arc<dyn Filesystem>,
    cwd: PathBuf,
}

impl SandboxBackedFilesystem {
    pub fn new(inner: Arc<dyn Filesystem>, cwd: impl Into<PathBuf>) -> Self {
        Self { inner, cwd: cwd.into() }
    }

    pub fn inner(&self) -> &dyn Filesystem {
        self.inner.as_ref()
    }
}

#[async_trait]
impl Filesystem for SandboxBackedFilesystem {
    async fn ls(&self, ctx: &RuntimeContext, path: &str) -> Result<Vec<FileInfo>> {
        self.inner.ls(ctx, path).await
    }
    async fn read(&self, ctx: &RuntimeContext, path: &str, offset: usize, limit: usize) -> Result<FileData> {
        self.inner.read(ctx, path, offset, limit).await
    }
    async fn write(&self, ctx: &RuntimeContext, path: &str, content: &str) -> Result<()> {
        self.inner.write(ctx, path, content).await
    }
    async fn edit(&self, ctx: &RuntimeContext, path: &str, old: &str, new: &str, replace_all: bool) -> Result<usize> {
        self.inner.edit(ctx, path, old, new, replace_all).await
    }
    async fn grep(&self, ctx: &RuntimeContext, pattern: &str, path: Option<&str>, glob: Option<&str>) -> Result<Vec<GrepMatch>> {
        self.inner.grep(ctx, pattern, path, glob).await
    }
    async fn glob(&self, ctx: &RuntimeContext, pattern: &str, path: &str) -> Result<Vec<FileInfo>> {
        self.inner.glob(ctx, pattern, path).await
    }
    async fn delete(&self, ctx: &RuntimeContext, path: &str) -> Result<()> {
        self.inner.delete(ctx, path).await
    }
    async fn move_path(&self, ctx: &RuntimeContext, from: &str, to: &str) -> Result<()> {
        self.inner.move_path(ctx, from, to).await
    }
    async fn exists(&self, ctx: &RuntimeContext, path: &str) -> Result<bool> {
        self.inner.exists(ctx, path).await
    }
}

#[async_trait]
impl SandboxFilesystem for SandboxBackedFilesystem {
    async fn execute(
        &self,
        _ctx: &RuntimeContext,
        command: &str,
        timeout_seconds: u32,
    ) -> Result<ExecuteResponse> {
        let mut cmd = if cfg!(windows) {
            let mut c = tokio::process::Command::new("cmd");
            c.arg("/C").arg(command);
            c
        } else {
            let mut c = tokio::process::Command::new("/bin/sh");
            c.arg("-c").arg(command);
            c
        };
        cmd.current_dir(&self.cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let fut = cmd.output();
        let out = match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_seconds.max(1) as u64),
            fut,
        )
        .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(Error::Io(e)),
            Err(_) => {
                return Ok(ExecuteResponse {
                    output: "Command timed out".into(),
                    exit_code: -1,
                    truncated: true,
                });
            }
        };
        let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.stderr.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&String::from_utf8_lossy(&out.stderr));
        }
        Ok(ExecuteResponse {
            output,
            exit_code: out.status.code().unwrap_or(-1),
            truncated: false,
        })
    }
}
