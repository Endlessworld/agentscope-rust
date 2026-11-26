//! Sandbox-aware filesystem: adds shell execution on top of the base `Filesystem` API,
//! mirroring `AbstractSandboxFilesystem`. The default backend runs commands on the local host.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agentscope_core::agent::RuntimeContext;
use agentscope_core::error::{Error, Result};
use agentscope_core::tool::{ProcessGuard, ProcessRegistry, TOOL_CALL_ID_CONTEXT_KEY};
use async_trait::async_trait;
use base64::Engine;
#[cfg(windows)]
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStdin};

use super::CompositeFilesystem;
use super::{ExecuteResponse, FileData, FileInfo, Filesystem, FilesystemKind, GrepMatch};

/// Java parity: `LocalFilesystemWithShell` caps combined output at 100_000 bytes.
///
/// Without it one runaway command can emit an unbounded tool result, and a single result
/// that large is enough to wedge the journal recorder's pending buffer for the rest of the
/// turn: the following required `state/checkpoint` append is then rejected and the whole
/// batch never commits.
const MAX_OUTPUT_BYTES: usize = 100_000;

/// 推送合批阈值：攒够 32KB 或 50ms 就先发一块。
///
/// 按行输出的命令每次 `read()` 只回来几十字节；逐块推送时 5MB 输出会变成 20 万+ 帧，
/// 每帧大约 280B 的 JSON 开销，客户端还要为每一帧整串重解析/重布局（O(n²)）。
const STREAM_BATCH_BYTES: usize = 32 * 1024;
/// 见 [`STREAM_BATCH_BYTES`]：不足一批的数据最多压这么久就发出去。
const STREAM_BATCH_DELAY: Duration = Duration::from_millis(50);

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
    /// Isolation scope the shell working directory is scoped by (Java parity gap #2329).
    ///
    /// `None` keeps the historical behaviour: every caller executes in `cwd`. With a scope
    /// set, `execute` runs in `cwd/<namespace segments>` — the same per-user / per-session
    /// sub-directory the filesystem layer namespaces file access by, so two users sharing
    /// one sandbox do not execute in one another's directory.
    isolation_scope: Option<super::super::agent::IsolationScope>,
    /// Session process registry (ACP v2): lets a client cancel kill the running command and
    /// lets its output stream while it runs. `None` keeps the historical fire-and-wait.
    processes: Option<Arc<ProcessRegistry>>,
    /// Windows-only pool of pre-warmed one-shot PowerShell processes.
    #[cfg(windows)]
    pwsh_pool: Option<Arc<super::PwshProcessPool>>,
}

impl SandboxBackedFilesystem {
    pub fn new(inner: Arc<dyn Filesystem>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            inner,
            cwd: cwd.into(),
            isolation_scope: None,
            processes: None,
            #[cfg(windows)]
            pwsh_pool: None,
        }
    }

    /// Attaches a pool of pre-warmed Windows PowerShell processes.
    #[cfg(windows)]
    pub fn with_pwsh_pool(mut self, pool: Arc<super::PwshProcessPool>) -> Self {
        self.pwsh_pool = Some(pool);
        self
    }

    /// Scopes shell execution by the given isolation scope.
    pub fn with_isolation_scope(mut self, scope: super::super::agent::IsolationScope) -> Self {
        self.isolation_scope = Some(scope);
        self
    }

    /// Registers the session process registry so shell commands are cancellable and
    /// stream their output.
    pub fn with_processes(mut self, processes: Arc<ProcessRegistry>) -> Self {
        self.processes = Some(processes);
        self
    }

    pub fn inner(&self) -> &dyn Filesystem {
        self.inner.as_ref()
    }

    /// Directory shell commands run from.
    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    /// Directory shell commands run from for this call.
    ///
    /// Resolves the isolation namespace (user / session) under [`cwd`](Self::cwd) when a
    /// scope is configured; without a scope, or without identifiers, it is `cwd` itself.
    pub fn scoped_cwd(&self, ctx: &RuntimeContext) -> PathBuf {
        let Some(scope) = self.isolation_scope else {
            return self.cwd.clone();
        };
        let namespace = scope.namespace(ctx.user_id(), ctx.session_id());
        namespace
            .iter()
            .fold(self.cwd.clone(), |path, segment| path.join(segment))
    }
}

#[async_trait]
impl Filesystem for SandboxBackedFilesystem {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn kind(&self) -> super::FilesystemKind {
        super::FilesystemKind::Sandbox
    }

    async fn ls(&self, ctx: &RuntimeContext, path: &str) -> Result<Vec<FileInfo>> {
        self.inner.ls(ctx, path).await
    }
    async fn read(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<FileData> {
        self.inner.read(ctx, path, offset, limit).await
    }
    async fn write(&self, ctx: &RuntimeContext, path: &str, content: &str) -> Result<()> {
        self.inner.write(ctx, path, content).await
    }
    async fn edit(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<usize> {
        self.inner.edit(ctx, path, old, new, replace_all).await
    }
    async fn grep(
        &self,
        ctx: &RuntimeContext,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>> {
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
    // pwsh 写管道时默认沿用控制台输出编码（中文系统为 GBK），客户端按 UTF-8 解码会得到成片
    // U+FFFD——终端流式输出和工具结果都受影响。开跑前把 stdout/stderr 与管道编码切到 UTF-8。
    let script = format!(
        "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; $OutputEncoding = [System.Text.Encoding]::UTF8; {}",
        command
    );
    // pwsh -EncodedCommand 要求 UTF-16LE 编码后再 Base64
    let utf16 = script.encode_utf16().collect::<Vec<u16>>();
    let byte_vec: Vec<u8> = utf16.iter().flat_map(|w| w.to_le_bytes()).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(&byte_vec);
    vec![
        "pwsh".into(),
        "-NoProfile".into(),
        "-EncodedCommand".into(),
        encoded,
    ]
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
        ctx: &RuntimeContext,
        command: &str,
        timeout_seconds: u32,
    ) -> Result<ExecuteResponse> {
        let scoped_cwd = self.scoped_cwd(ctx);
        // The per-scope directory is created on demand: the shell is the first caller for a
        // fresh user/session, and `Command::current_dir` fails on a missing directory.
        let _ = std::fs::create_dir_all(&scoped_cwd);

        // Take a pre-warmed one-shot PowerShell when this is an ACP session that opted into
        // pooling. A miss falls through to the historical cold-spawn path, so pool warmup or
        // exhaustion never changes command semantics.
        #[cfg(windows)]
        if let Some(pool) = self.pwsh_pool.as_ref() {
            if let Some(warm) = pool.checkout().await {
                let shell = RunningShell {
                    child: warm.child,
                    stdin: Some(warm.stdin),
                    stdout: Some(Box::new(warm.stdout)),
                    stderr: Some(Box::new(warm.stderr)),
                };
                return self
                    .run_shell(shell, ctx, command, &scoped_cwd, timeout_seconds)
                    .await;
            }
        }

        // `build_windows_command` 只在 Windows 下存在，所以这里必须用 `#[cfg]` 分支，
        // 不能用 `if cfg!(windows)`：后者两个分支都会被编译，Windows 专属函数在
        // 非 Windows 目标上会直接 `cannot find function` 编译失败。
        #[cfg(windows)]
        let mut cmd = {
            let args = build_windows_command(command);
            let mut c = tokio::process::Command::new(&args[0]);
            c.args(&args[1..]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = tokio::process::Command::new("/bin/sh");
            c.arg("-c").arg(command);
            c
        };
        cmd.current_dir(&scoped_cwd)
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
        // An interrupted turn drops this future: without `kill_on_drop` the child would
        // keep running (and keep holding the pipes) after `session/cancel`.
        let mut child = cmd.kill_on_drop(true).spawn().map_err(|e| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to spawn command: {}", e),
            ))
        })?;
        let stdout = child
            .stdout
            .take()
            .map(|stream| Box::new(stream) as Box<dyn AsyncRead + Unpin + Send>);
        let stderr = child
            .stderr
            .take()
            .map(|stream| Box::new(stream) as Box<dyn AsyncRead + Unpin + Send>);
        let shell = RunningShell {
            child,
            stdin: None,
            stdout,
            stderr,
        };
        self.run_shell(shell, ctx, command, &scoped_cwd, timeout_seconds)
            .await
    }
}

/// A spawned shell plus its pipes.
///
/// Cold spawns put the command in `pwsh -EncodedCommand`; warm processes leave `stdin` open and
/// receive the command through it. Everything after registration is shared so the two paths
/// cannot drift.
struct RunningShell {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<Box<dyn AsyncRead + Unpin + Send>>,
    stderr: Option<Box<dyn AsyncRead + Unpin + Send>>,
}

impl SandboxBackedFilesystem {
    async fn run_shell(
        &self,
        shell: RunningShell,
        ctx: &RuntimeContext,
        command: &str,
        cwd: &std::path::Path,
        timeout_seconds: u32,
    ) -> Result<ExecuteResponse> {
        let RunningShell {
            mut child,
            mut stdin,
            stdout,
            stderr,
        } = shell;
        #[cfg(not(windows))]
        let _ = stdin.take();
        let pid = child.id().unwrap_or(0);

        // A warm process receives the command through stdin. Send it *before* registering the
        // process: otherwise a client cancel can observe the registration and kill the shell
        // while this write is in flight, turning a valid cancellation into BrokenPipe.
        #[cfg(windows)]
        if let Some(stdin) = stdin.as_mut() {
            if let Err(error) = send_warm_command(stdin, cwd, command).await {
                kill_process_tree(pid);
                let _ = child.kill().await;
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    format!("failed to hand the command to the warm shell: {error}"),
                )));
            }
        }

        // Register the child so a client cancel (tool_call/cancel / terminal/cancel) can kill
        // the whole tree, and stream its output while it runs. Both are no-ops without a
        // registry (v1 hosts) or a tool-call id.
        let process_key = ctx
            .extension::<String>(TOOL_CALL_ID_CONTEXT_KEY)
            .map(|id| id.as_str().to_string());
        let _guard: Option<ProcessGuard> = match (self.processes.as_ref(), process_key.as_ref()) {
            (Some(registry), Some(key)) => {
                let registry = Arc::clone(registry);
                Some(registry.register(key.clone(), move || kill_process_tree(pid)))
            }
            _ => None,
        };
        let stream_target = match (self.processes.as_ref(), process_key.as_ref()) {
            (Some(registry), Some(key)) => Some((Arc::clone(registry), key.clone())),
            _ => None,
        };
        if let (Some(registry), Some(key)) = (self.processes.as_ref(), process_key.as_ref()) {
            registry.emit_started(key, command, &cwd.to_string_lossy());
        }

        // Spawn background tasks to drain stdout/stderr concurrently, keeping at most
        // `MAX_OUTPUT_BYTES` each while still reading to EOF so the child never blocks on a
        // full pipe.
        let stdout_sink = stream_target.clone();
        let stdout_fut = async move {
            match stdout {
                Some(stream) => read_capped(stream, MAX_OUTPUT_BYTES, stdout_sink).await,
                None => (Vec::new(), false),
            }
        };
        let stderr_sink = stream_target;
        let stderr_fut = async move {
            match stderr {
                Some(stream) => read_capped(stream, MAX_OUTPUT_BYTES, stderr_sink).await,
                None => (Vec::new(), false),
            }
        };
        let stdout_handle = tokio::spawn(stdout_fut);
        let stderr_handle = tokio::spawn(stderr_fut);

        // Wait for exit with timeout.
        let timeout = Duration::from_secs(timeout_seconds.max(1) as u64);
        let exit_status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(result) => result.map_err(Error::Io)?,
            Err(_elapsed) => {
                // Timeout: kill the entire process tree to avoid orphans.
                if let (Some(registry), Some(key)) = (self.processes.as_ref(), process_key.as_ref())
                {
                    registry.emit_exited(key, ExecuteResponse::TIMEOUT_EXIT_CODE);
                }
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
                    output: format!(
                        "Command timed out after {} seconds. For long-running commands, re-run \
                         using the timeout parameter.",
                        timeout_seconds.max(1)
                    ),
                    // A confirmed timeout, not a process result: the flag is what the shell tool
                    // reports to the model, the exit code only mirrors `timeout(1)`'s convention.
                    exit_code: ExecuteResponse::TIMEOUT_EXIT_CODE,
                    truncated: true,
                    timed_out: true,
                    cancelled: false,
                });

            }
        };

        // Collect drained output.
        let (stdout_bytes, stdout_truncated) = stdout_handle.await.unwrap_or_else(|e| {
            tracing::warn!("stdout task join error: {}", e);
            (Vec::new(), false)
        });
        let (stderr_bytes, stderr_truncated) = stderr_handle.await.unwrap_or_else(|e| {
            tracing::warn!("stderr task join error: {}", e);
            (Vec::new(), false)
        });

        let mut output = String::from_utf8_lossy(&stdout_bytes).into_owned();
        if !stderr_bytes.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&String::from_utf8_lossy(&stderr_bytes));
        }
        let truncated = cap_output(&mut output, stdout_truncated || stderr_truncated);

        let cancelled = _guard
            .as_ref()
            .map(|guard| guard.is_cancelled())
            .unwrap_or(false);
        if let (Some(registry), Some(key)) = (self.processes.as_ref(), process_key.as_ref()) {
            registry.emit_exited(key, exit_status.code().unwrap_or(-1));
        }
        Ok(ExecuteResponse {
            output,
            exit_code: exit_status.code().unwrap_or(-1),
            truncated,
            timed_out: false,
            cancelled,
        })
    }
}

/// Hands a warmed shell its cwd and one command, then closes its stdin.
#[cfg(windows)]
async fn send_warm_command(
    stdin: &mut ChildStdin,
    cwd: &std::path::Path,
    command: &str,
) -> std::io::Result<()> {
    let cwd = base64::engine::general_purpose::STANDARD.encode(cwd.to_string_lossy().as_bytes());
    let command = base64::engine::general_purpose::STANDARD.encode(command.as_bytes());
    stdin.write_all(cwd.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.write_all(command.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    stdin.shutdown().await
}

/// Reads a stream to EOF, keeping at most `cap` bytes but draining the rest so the child
/// process never blocks on a full pipe.
async fn read_capped<R>(
    mut stream: R,
    cap: usize,
    sink: Option<(Arc<ProcessRegistry>, String)>,
) -> (Vec<u8>, bool)
where
    R: AsyncReadExt + Unpin,
{
    let mut collected = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 8192];
    // 待发送的推送缓冲：既用于合批，也用于扣住尾部不完整的多字节 UTF-8 序列
    // （否则客户端逐 chunk 解码会把一个汉字拆成两个 U+FFFD）。
    let mut pending: Vec<u8> = Vec::new();
    let mut flush_at: Option<tokio::time::Instant> = None;
    loop {
        let read = match flush_at {
            // 没攒够阈值时最多再等 STREAM_BATCH_DELAY：打印一行后长时间无声的命令
            // 也要及时显示，不能等下一次写入。
            Some(at) => tokio::select! {
                read = stream.read(&mut buf) => read,
                _ = tokio::time::sleep_until(at) => {
                    flush_stream(sink.as_ref(), &mut pending, false);
                    flush_at = None;
                    continue;
                }
            },
            None => stream.read(&mut buf).await,
        };
        let read = match read {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        // Forward the output before capping: a cancelled or truncated command still shows the
        // client everything it produced.
        if sink.is_some() {
            pending.extend_from_slice(&buf[..read]);
            if pending.len() >= STREAM_BATCH_BYTES {
                flush_stream(sink.as_ref(), &mut pending, false);
                flush_at = None;
            } else if flush_at.is_none() {
                flush_at = Some(tokio::time::Instant::now() + STREAM_BATCH_DELAY);
            }
        }
        let room = cap.saturating_sub(collected.len());
        if room == 0 {
            // Anything read now is dropped: the output really is longer than the cap.
            truncated = true;
        } else {
            collected.extend_from_slice(&buf[..read.min(room)]);
            truncated |= read > room;
        }
    }
    flush_stream(sink.as_ref(), &mut pending, true);
    (collected, truncated)
}

/// 发掉缓冲里完整的 UTF-8 前缀。
///
/// `at_eof` 为真时连尾部残留（半截字符 / 非法字节）也原样发出，避免命令结束时丢内容；
/// 否则只把超过一个不完整序列最大长度的非法字节原样发掉（GBK、二进制输出），
/// 让客户端按 lossy 处理，又不会无限积压。
fn flush_stream(
    sink: Option<&(Arc<ProcessRegistry>, String)>,
    pending: &mut Vec<u8>,
    at_eof: bool,
) {
    let Some((registry, key)) = sink else {
        return;
    };
    if pending.is_empty() {
        return;
    }
    let complete = utf8_prefix_len(pending);
    if complete > 0 {
        registry.emit_output(key, &pending[..complete]);
        pending.drain(..complete);
    }
    if !pending.is_empty() && (at_eof || pending.len() > 4) {
        registry.emit_output(key, pending);
        pending.clear();
    }
}

/// bytes 中完整 UTF-8 前缀的长度；尾部不完整序列与非法字节不计入。
fn utf8_prefix_len(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(err) => err.valid_up_to(),
    }
}

/// Caps the combined output the way `LocalFilesystemWithShell` does: cut on a char boundary
/// and append the same marker, so the model knows the result is incomplete.
///
/// `already_truncated` marks a stream that was capped while draining; its cut may not be
/// visible in the lossily decoded string, so the marker is still appended.
fn cap_output(output: &mut String, already_truncated: bool) -> bool {
    if output.len() <= MAX_OUTPUT_BYTES && !already_truncated {
        return false;
    }
    if output.len() > MAX_OUTPUT_BYTES {
        let mut end = MAX_OUTPUT_BYTES;
        while end > 0 && !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
    }
    output.push_str(&format!(
        "\n\n... Output truncated at {MAX_OUTPUT_BYTES} bytes."
    ));
    true
}

// ---------------------------------------------------------------------------
// PinnedSandboxFilesystem
// ---------------------------------------------------------------------------

/// A [`SandboxFilesystem`] pinned to one already-acquired sandbox.
///
/// Mirrors Java `PinnedSandboxFilesystem`: the instance holds the sandbox filesystem it was
/// created from for its whole lifetime, so an out-of-call consumer (the async session mirror)
/// never re-enters the manager and never acquires a second sandbox. [`release`](Self::release)
/// publishes a latch that rejects every later operation, because a self-managed sandbox is
/// gone once its call releases it.
pub struct PinnedSandboxFilesystem {
    inner: Arc<dyn SandboxFilesystem>,
    released: Arc<std::sync::atomic::AtomicBool>,
}

impl PinnedSandboxFilesystem {
    pub fn new(inner: Arc<dyn SandboxFilesystem>) -> Self {
        Self {
            inner,
            released: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// The pinned sandbox filesystem. It is the same instance the pin was built from.
    pub fn inner(&self) -> &Arc<dyn SandboxFilesystem> {
        &self.inner
    }

    /// Publishes the release latch. Idempotent; unlike Java's
    /// `clearSandboxIfCurrent` it never unpins the inner filesystem.
    pub fn release(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_released(&self) -> bool {
        self.released.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn ensure_live(&self) -> Result<()> {
        if self.is_released() {
            return Err(Error::Tool("Sandbox has been released".into()));
        }
        Ok(())
    }
}

#[async_trait]
impl Filesystem for PinnedSandboxFilesystem {
    /// Returns the **inner** sandbox backend, not this wrapper.
    ///
    /// Prompt building looks for a `SandboxBackedFilesystem` to report the sandbox root; a
    /// wrapper that only answered for itself made that lookup fail and fall back to the
    /// host workspace, so the prompt named the host directory while telling the model that
    /// the host is unreachable (contradiction). Forwarding keeps the sandbox root visible.
    fn as_any(&self) -> &dyn std::any::Any {
        self.inner.as_any()
    }

    fn kind(&self) -> FilesystemKind {
        FilesystemKind::Sandbox
    }

    async fn ls(&self, ctx: &RuntimeContext, path: &str) -> Result<Vec<FileInfo>> {
        self.ensure_live()?;
        self.inner.ls(ctx, path).await
    }

    async fn read(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<FileData> {
        self.ensure_live()?;
        self.inner.read(ctx, path, offset, limit).await
    }

    async fn write(&self, ctx: &RuntimeContext, path: &str, content: &str) -> Result<()> {
        self.ensure_live()?;
        self.inner.write(ctx, path, content).await
    }

    async fn edit(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<usize> {
        self.ensure_live()?;
        self.inner.edit(ctx, path, old, new, replace_all).await
    }

    async fn grep(
        &self,
        ctx: &RuntimeContext,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>> {
        self.ensure_live()?;
        self.inner.grep(ctx, pattern, path, glob).await
    }

    async fn glob(&self, ctx: &RuntimeContext, pattern: &str, path: &str) -> Result<Vec<FileInfo>> {
        self.ensure_live()?;
        self.inner.glob(ctx, pattern, path).await
    }

    async fn delete(&self, ctx: &RuntimeContext, path: &str) -> Result<()> {
        self.ensure_live()?;
        self.inner.delete(ctx, path).await
    }

    async fn move_path(&self, ctx: &RuntimeContext, from: &str, to: &str) -> Result<()> {
        self.ensure_live()?;
        self.inner.move_path(ctx, from, to).await
    }

    async fn exists(&self, ctx: &RuntimeContext, path: &str) -> Result<bool> {
        self.ensure_live()?;
        self.inner.exists(ctx, path).await
    }

    async fn sync(&self, ctx: &RuntimeContext, path: &str) -> Result<()> {
        self.ensure_live()?;
        self.inner.sync(ctx, path).await
    }
}

#[async_trait]
impl SandboxFilesystem for PinnedSandboxFilesystem {
    async fn execute(
        &self,
        ctx: &RuntimeContext,
        command: &str,
        timeout_seconds: u32,
    ) -> Result<ExecuteResponse> {
        self.ensure_live()?;
        self.inner.execute(ctx, command, timeout_seconds).await
    }
}

// ---------------------------------------------------------------------------
// RoutedSandboxFilesystem
// ---------------------------------------------------------------------------

/// Prefix-routed filesystem that keeps shell execution on its primary sandbox.
///
/// Mirrors Java `RoutedSandboxFilesystem`: file operations go through a
/// [`CompositeFilesystem`] (primary as the default backend plus optional routes, longest
/// prefix first), while `execute` always reaches the primary. A caller can therefore mount
/// extra stores (e.g. `memory-stores/`) without losing `shell_execute`.
pub struct RoutedSandboxFilesystem {
    primary: Arc<dyn SandboxFilesystem>,
    composite: CompositeFilesystem,
}

impl RoutedSandboxFilesystem {
    pub fn new(
        primary: Arc<dyn SandboxFilesystem>,
        routes: Vec<(String, Arc<dyn Filesystem>)>,
    ) -> Self {
        let composite =
            CompositeFilesystem::new(Arc::clone(&primary) as Arc<dyn Filesystem>, routes);
        Self { primary, composite }
    }

    /// The primary sandbox filesystem (the lifecycle middleware injects into this).
    pub fn primary(&self) -> &Arc<dyn SandboxFilesystem> {
        &self.primary
    }
}

#[async_trait]
impl Filesystem for RoutedSandboxFilesystem {
    /// Returns the **primary** sandbox backend (see `PinnedSandboxFilesystem::as_any`).
    fn as_any(&self) -> &dyn std::any::Any {
        self.primary.as_any()
    }

    fn kind(&self) -> FilesystemKind {
        FilesystemKind::Sandbox
    }

    async fn ls(&self, ctx: &RuntimeContext, path: &str) -> Result<Vec<FileInfo>> {
        self.composite.ls(ctx, path).await
    }

    async fn read(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<FileData> {
        self.composite.read(ctx, path, offset, limit).await
    }

    async fn write(&self, ctx: &RuntimeContext, path: &str, content: &str) -> Result<()> {
        self.composite.write(ctx, path, content).await
    }

    async fn edit(
        &self,
        ctx: &RuntimeContext,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<usize> {
        self.composite.edit(ctx, path, old, new, replace_all).await
    }

    async fn grep(
        &self,
        ctx: &RuntimeContext,
        pattern: &str,
        path: Option<&str>,
        glob: Option<&str>,
    ) -> Result<Vec<GrepMatch>> {
        self.composite.grep(ctx, pattern, path, glob).await
    }

    async fn glob(&self, ctx: &RuntimeContext, pattern: &str, path: &str) -> Result<Vec<FileInfo>> {
        self.composite.glob(ctx, pattern, path).await
    }

    async fn delete(&self, ctx: &RuntimeContext, path: &str) -> Result<()> {
        self.composite.delete(ctx, path).await
    }

    async fn move_path(&self, ctx: &RuntimeContext, from: &str, to: &str) -> Result<()> {
        self.composite.move_path(ctx, from, to).await
    }

    async fn exists(&self, ctx: &RuntimeContext, path: &str) -> Result<bool> {
        self.composite.exists(ctx, path).await
    }

    async fn sync(&self, ctx: &RuntimeContext, path: &str) -> Result<()> {
        self.composite.sync(ctx, path).await
    }
}

#[async_trait]
impl SandboxFilesystem for RoutedSandboxFilesystem {
    async fn execute(
        &self,
        ctx: &RuntimeContext,
        command: &str,
        timeout_seconds: u32,
    ) -> Result<ExecuteResponse> {
        self.primary.execute(ctx, command, timeout_seconds).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::LocalFilesystem;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("asr_fs_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sandbox_fs(dir: &std::path::Path) -> Arc<dyn SandboxFilesystem> {
        Arc::new(SandboxBackedFilesystem::new(
            Arc::new(LocalFilesystem::sandboxed(dir)),
            dir,
        ))
    }

    /// The shell working directory is resolved per isolation scope so two users sharing one
    /// sandbox do not execute in one another's directory (#2329).
    #[test]
    fn scoped_cwd_follows_the_isolation_scope() {
        let root = temp_dir("scoped_cwd");
        let build = || -> SandboxBackedFilesystem {
            SandboxBackedFilesystem::new(
                Arc::new(LocalFilesystem::sandboxed(&root)) as Arc<dyn Filesystem>,
                root.clone(),
            )
        };
        let ctx = RuntimeContext::default()
            .with_session_id("s-1")
            .with_user_id("u-1");

        // No scope configured: the historical shared root is kept.
        assert_eq!(build().scoped_cwd(&ctx), root);

        let session = build().with_isolation_scope(crate::agent::IsolationScope::Session);
        assert_eq!(session.scoped_cwd(&ctx), root.join("s-1"));

        let user = build().with_isolation_scope(crate::agent::IsolationScope::User);
        assert_eq!(user.scoped_cwd(&ctx), root.join("u-1"));

        // An anonymous caller (no identifiers) falls back to the shared root.
        let anonymous = build().with_isolation_scope(crate::agent::IsolationScope::Session);
        assert_eq!(anonymous.scoped_cwd(&RuntimeContext::default()), root);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Wrappers forward `as_any` to the inner sandbox backend so a caller can still find the
    /// sandbox root (and cannot be answered with the wrapper type instead).
    #[test]
    fn sandbox_wrappers_expose_the_inner_backend() {
        let dir = temp_dir("as_any");
        let inner = sandbox_fs(&dir);
        let pinned = PinnedSandboxFilesystem::new(Arc::clone(&inner));
        assert!(
            pinned
                .as_any()
                .downcast_ref::<SandboxBackedFilesystem>()
                .is_some(),
            "a pinned filesystem must answer for its sandbox backend"
        );
        let routed = RoutedSandboxFilesystem::new(Arc::clone(&inner), Vec::new());
        assert!(routed
            .as_any()
            .downcast_ref::<SandboxBackedFilesystem>()
            .is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_command_that_exits_with_124_on_its_own_is_not_a_timeout() {
        // `timeout 30 build` (or a script's own `exit 124`) is a normal result: only the
        // execution path's deadline may set the timeout flag.
        let response = ExecuteResponse {
            output: "done".to_string(),
            exit_code: ExecuteResponse::TIMEOUT_EXIT_CODE,
            truncated: false,
            timed_out: false,
            cancelled: false,
        };
        assert!(!response.timed_out());
        assert!(!response.is_success());
    }

    #[tokio::test]
    async fn pinned_keeps_the_acquired_filesystem_and_rejects_after_release() {
        let dir = temp_dir("pinned");
        let inner = sandbox_fs(&dir);
        let pinned = PinnedSandboxFilesystem::new(Arc::clone(&inner));
        assert!(Arc::ptr_eq(pinned.inner(), &inner), "must not re-acquire");

        let ctx = RuntimeContext::default();
        pinned.write(&ctx, "mirror.txt", "hello").await.unwrap();
        assert_eq!(
            pinned
                .read(&ctx, "mirror.txt", 0, 10)
                .await
                .unwrap()
                .content,
            "hello"
        );
        assert!(!pinned.is_released());

        pinned.release();
        pinned.release(); // idempotent
        assert!(pinned.is_released());
        let error = pinned.read(&ctx, "mirror.txt", 0, 10).await.unwrap_err();
        assert!(error.to_string().contains("released"), "{error}");
        assert!(pinned.exists(&ctx, "mirror.txt").await.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn routed_sends_prefixed_paths_to_the_route_and_keeps_execute_on_primary() {
        let primary_dir = temp_dir("routed_primary");
        let memory_dir = temp_dir("routed_memory");
        std::fs::write(memory_dir.join("m.md"), "from the store").unwrap();

        let primary = sandbox_fs(&primary_dir);
        let routed = RoutedSandboxFilesystem::new(
            Arc::clone(&primary),
            vec![(
                "memory/".to_string(),
                Arc::new(LocalFilesystem::sandboxed(&memory_dir)) as Arc<dyn Filesystem>,
            )],
        );
        assert!(Arc::ptr_eq(routed.primary(), &primary));

        let ctx = RuntimeContext::default();
        assert_eq!(
            routed
                .read(&ctx, "memory/m.md", 0, 100)
                .await
                .unwrap()
                .content,
            "from the store"
        );
        routed.write(&ctx, "notes.txt", "primary").await.unwrap();
        assert!(primary_dir.join("notes.txt").exists());
        assert!(!memory_dir.join("notes.txt").exists());

        let _ = std::fs::remove_dir_all(&primary_dir);
        let _ = std::fs::remove_dir_all(&memory_dir);
    }

    #[tokio::test]
    async fn execute_caps_runaway_output_at_the_java_parity_limit() {
        // A single tool result this large used to exhaust the journal pending buffer and
        // wedge the rest of the turn (the required checkpoint could not be appended).
        #[cfg(windows)]
        let command = "'x' * 200000";
        #[cfg(not(windows))]
        let command = "head -c 200000 /dev/zero | tr '\\0' x";

        let dir = temp_dir("capped");
        let fs = sandbox_fs(&dir);
        let response = fs
            .execute(&RuntimeContext::default(), command, 120)
            .await
            .expect("execute");

        assert_eq!(response.exit_code, 0);
        assert!(response.truncated, "oversized output must be flagged");
        assert!(
            response.output.starts_with(&"x".repeat(MAX_OUTPUT_BYTES)),
            "the first {MAX_OUTPUT_BYTES} bytes are kept verbatim"
        );
        assert!(
            response.output.ends_with(&format!(
                "\n\n... Output truncated at {MAX_OUTPUT_BYTES} bytes."
            )),
            "the model must be told the result is incomplete"
        );
        // Only the retained prefix plus the marker: the rest was drained, not accumulated.
        assert_eq!(
            response.output.len(),
            MAX_OUTPUT_BYTES
                + format!("\n\n... Output truncated at {MAX_OUTPUT_BYTES} bytes.").len()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn execute_keeps_small_output_untruncated() {
        #[cfg(windows)]
        let command = "'hello'";
        #[cfg(not(windows))]
        let command = "printf hello";

        let dir = temp_dir("small");
        let fs = sandbox_fs(&dir);
        let response = fs
            .execute(&RuntimeContext::default(), command, 120)
            .await
            .expect("execute");

        assert_eq!(response.output.trim(), "hello");
        assert!(!response.truncated);
        assert!(!response.timed_out);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cap_output_cuts_on_a_char_boundary() {
        // 60_000 two-byte chars exceed the byte budget without ever landing on an ASCII
        // boundary at the cut point.
        let mut output = "é".repeat(60_000);
        assert!(cap_output(&mut output, false));

        let marker = format!("\n\n... Output truncated at {MAX_OUTPUT_BYTES} bytes.");
        let kept = &output[..output.len() - marker.len()];
        assert!(kept.len() <= MAX_OUTPUT_BYTES);
        assert!(kept.chars().all(|c| c == 'é'), "cut mid-char would panic");
        assert!(output.ends_with(&marker));
    }

    #[test]
    fn cap_output_marks_a_silently_capped_stream() {
        // The stream was capped while draining, so even a short lossily decoded string must
        // carry the marker.
        let mut output = "short".to_string();
        assert!(cap_output(&mut output, true));
        assert!(output.ends_with(&format!(
            "\n\n... Output truncated at {MAX_OUTPUT_BYTES} bytes."
        )));
    }
    /// 逐行输出必须合批：两百次小写入不能变成两百多帧（每帧都要客户端整串重渲染）。
    #[tokio::test]
    async fn streaming_coalesces_small_writes() {
        use std::sync::Mutex as StdMutex;
        use tokio::io::AsyncWriteExt;

        struct Recorder(Arc<StdMutex<Vec<Vec<u8>>>>);
        impl agentscope_core::tool::ProcessEvents for Recorder {
            fn output(&self, _key: &str, bytes: &[u8]) {
                self.0.lock().unwrap().push(bytes.to_vec());
            }
        }

        let (mut tx, rx) = tokio::io::duplex(1 << 20);
        let chunks = Arc::new(StdMutex::new(Vec::new()));
        let registry = agentscope_core::tool::ProcessRegistry::new();
        registry.set_events(Some(Arc::new(Recorder(Arc::clone(&chunks)))));
        let reader = tokio::spawn(read_capped(
            rx,
            MAX_OUTPUT_BYTES,
            Some((Arc::clone(&registry), "call-1".to_string())),
        ));

        let mut expected = Vec::new();
        for i in 0..200 {
            let line = format!("line-{i:04}-abcdefghijklmnopqrstuvwxyz\n");
            expected.extend_from_slice(line.as_bytes());
            tx.write_all(line.as_bytes()).await.unwrap();
            // 让出执行机会，逼出 200 次独立的 read（否则 duplex 会合并成一次大读）
            tokio::task::yield_now().await;
        }
        drop(tx);
        let (collected, truncated) = reader.await.unwrap();

        assert!(!truncated);
        assert_eq!(collected, expected);
        let sent = chunks.lock().unwrap();
        assert_eq!(sent.concat(), expected, "no byte may be lost or reordered");
        assert!(!sent.is_empty(), "no output was forwarded");
        assert!(sent.len() <= 8, "expected batched chunks, got {} sizes {:?}", sent.len(), sent.iter().map(|c| c.len()).collect::<Vec<_>>());
    }

    /// 不足一批的输出最多压 50ms：交互式命令打印提示后不会一直不显示。
    #[tokio::test]
    async fn streaming_flushes_after_the_batch_delay() {
        use std::sync::Mutex as StdMutex;
        use tokio::io::AsyncWriteExt;

        struct Recorder(Arc<StdMutex<Vec<Vec<u8>>>>);
        impl agentscope_core::tool::ProcessEvents for Recorder {
            fn output(&self, _key: &str, bytes: &[u8]) {
                self.0.lock().unwrap().push(bytes.to_vec());
            }
        }

        let (mut tx, rx) = tokio::io::duplex(1 << 20);
        let chunks = Arc::new(StdMutex::new(Vec::new()));
        let registry = agentscope_core::tool::ProcessRegistry::new();
        registry.set_events(Some(Arc::new(Recorder(Arc::clone(&chunks)))));
        let reader = tokio::spawn(read_capped(
            rx,
            MAX_OUTPUT_BYTES,
            Some((Arc::clone(&registry), "call-1".to_string())),
        ));

        tx.write_all(b"press any key").await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            !chunks.lock().unwrap().is_empty(),
            "a small write must flush on the 50ms timer"
        );

        drop(tx);
        let _ = reader.await.unwrap();
    }
}
