//! Sandbox-aware filesystem: adds shell execution on top of the base `Filesystem` API,
//! mirroring `AbstractSandboxFilesystem`. The default backend runs commands on the local host.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result};
use tokio::io::AsyncReadExt;

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

/// Windows once 模式：使用 pwsh -EncodedCommand 执行命令。
/// 不生成任何临时脚本文件（避免被安全软件识别为"落盘+执行"的可疑行为而频繁告警），
/// 而是将命令以 UTF-16LE 编码后做 Base64，作为 -EncodedCommand 参数传给 pwsh。
/// 优点：
///  1. Base64 为纯 ASCII，彻底规避 ProcessBuilder/Command 在 Windows 下以 ANSI 编码传参导致的中文乱码；
///  2. pwsh 没有 cmd 的 %VAR% 展开机制（% 原样保留，如 git log --format=%s 无需转义）；
///  3. 不落盘，无临时文件残留，降低安全软件告警频率。
#[cfg(windows)]
fn build_windows_command(command: &str) -> Vec<String> {
    // pwsh -EncodedCommand 要求 UTF-16LE 编码后再 Base64
    let utf16 = command.encode_utf16().collect::<Vec<u16>>();
    let byte_vec: Vec<u8> = utf16.iter().flat_map(|w| w.to_le_bytes()).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(&byte_vec);
    vec!["pwsh".into(), "-NoProfile".into(), "-EncodedCommand".into(), encoded]
}

/// Kills an entire process tree on Windows using `taskkill /F /T`.
///
/// This is critical: when we kill `pwsh.exe`, its child processes (`git.exe`,
/// `git-credential-manager.exe`, etc.) become orphans that hold pipe handles
/// open indefinitely, causing `tokio::process::Child::wait()` to hang forever.
/// `taskkill /T` kills the process tree recursively.
#[cfg(windows)]
pub(crate) fn kill_process_tree(pid: u32) {
    use std::process::Command as StdCommand;
    // Best-effort: don't unwrap — the process may have already exited.
    let _ = StdCommand::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
}

#[cfg(not(windows))]
pub(crate) fn kill_process_tree(pid: u32) {
    use std::process::Command as StdCommand;
    // On Unix, send SIGTERM to the process group.
    let _ = StdCommand::new("kill")
        .args(["-TERM", "--", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
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
            let args = build_windows_command(command);
            let mut c = tokio::process::Command::new(&args[0]);
            c.args(&args[1..]);
            c
        } else {
            let mut c = tokio::process::Command::new("/bin/sh");
            c.arg("-c").arg(command);
            c
        };
        cmd.current_dir(&self.cwd)
            // Never inherit the parent's stdin: when the agent runs over ACP
            // stdio, stdin is a pipe the client keeps open, and an inherited
            // pipe makes pwsh wait for input instead of exiting.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // ── Key fix: spawn() + wait() instead of cmd.output() ──
        //
        // `cmd.output()` wraps the Child handle inside an anonymous future's
        // state machine. When `tokio::time::timeout` fires and drops that future,
        // `Child::drop()` only kills the *direct* child (pwsh.exe), but its
        // grandchildren (git.exe, git-credential-manager.exe, etc.) become
        // orphans that hold pipe handles open → wait() hangs forever → the
        // tool appears to "timeout" even for trivial commands like `git --version`.
        //
        // By spawning first, the Child handle stays in scope and we can:
        // 1. Take stdout/stderr BEFORE waiting (prevents pipe-full deadlock).
        // 2. On timeout, kill the ENTIRE process tree via `taskkill /T`.
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to spawn command: {}", e),
            )))?;

        let pid = child.id().unwrap_or(0);

        // Take the piped streams BEFORE awaiting — if we don't, and the child
        // writes enough to fill the OS pipe buffer (~64KB on Windows), it blocks
        // and our `wait()` never returns.
        let mut stdout_stream = child.stdout.take();
        let mut stderr_stream = child.stderr.take();

        // Spawn background tasks to drain stdout/stderr concurrently.
        let stdout_fut = async move {
            let mut buf = Vec::new();
            if let Some(ref mut stream) = stdout_stream {
                let _ = stream.read_to_end(&mut buf).await;
            }
            buf
        };
        let stderr_fut = async move {
            let mut buf = Vec::new();
            if let Some(ref mut stream) = stderr_stream {
                let _ = stream.read_to_end(&mut buf).await;
            }
            buf
        };
        let stdout_handle = tokio::spawn(stdout_fut);
        let stderr_handle = tokio::spawn(stderr_fut);

        // Wait for exit with timeout.
        let timeout = Duration::from_secs(timeout_seconds.max(1) as u64);
        let exit_status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(result) => result.map_err(Error::Io)?,
            Err(_elapsed) => {
                // Timeout: kill the entire process tree to avoid orphans.
                kill_process_tree(pid);
                // Also kill the direct child as a fallback.
                let _ = child.kill().await;
                // Wait for drain tasks to finish so we don't leak tasks.
                let _ = tokio::time::timeout(Duration::from_secs(2), async {
                    let _ = stdout_handle.await;
                    let _ = stderr_handle.await;
                })
                .await;
                return Ok(ExecuteResponse {
                    output: "Command timed out".into(),
                    exit_code: -1,
                    truncated: true,
                });
            }
        };

        // Collect drained output.
        let stdout_bytes = stdout_handle
            .await
            .unwrap_or_else(|e| { tracing::warn!("stdout task join error: {}", e); Vec::new() });
        let stderr_bytes = stderr_handle
            .await
            .unwrap_or_else(|e| { tracing::warn!("stderr task join error: {}", e); Vec::new() });

        let mut output = String::from_utf8_lossy(&stdout_bytes).into_owned();
        if !stderr_bytes.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&String::from_utf8_lossy(&stderr_bytes));
        }

        Ok(ExecuteResponse {
            output,
            exit_code: exit_status.code().unwrap_or(-1),
            truncated: false,
        })
    }
}
