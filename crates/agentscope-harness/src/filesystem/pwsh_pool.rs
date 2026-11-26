//! Pre-warmed, one-shot `pwsh` processes.
//!
//! Starting PowerShell costs 300–400ms even with `-NoProfile`, which dominates short `execute`
//! calls. A pooled process is started ahead of time with a tiny bootstrap that announces
//! readiness and then waits for exactly one command on stdin. Checkout consumes that process;
//! it exits normally (EOF is the completion signal) and a replacement is already warming in the
//! background. No shell state is reused: every command still runs in a brand-new process.
//!
//! The pool does not replace `build_windows_command`; the cold path remains the fallback when
//! the pool is empty, warming, or not configured.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Notify;
use tokio::time::Instant;

/// Printed by the bootstrap before it blocks on stdin. The pool only hands out processes that
/// emitted it, otherwise the first command would merely pay the same startup latency.
const READY_MARKER: &str = "__AGENTSCOPE_PWSH_READY__";

/// A warm process that never became ready is discarded; this is generous enough for a cold
/// .NET start on a busy machine and still bounds a stuck child.
const WARMUP_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a checkout waits for a ready process before falling back to the cold path. Waiting
/// longer than a cold spawn would be strictly worse; replacements keep warming meanwhile.
const CHECKOUT_WAIT: Duration = Duration::from_millis(50);

/// One pre-warmed, unused shell.
pub(crate) struct WarmPwsh {
    pub(crate) child: Child,
    pub(crate) stdin: ChildStdin,
    /// The bootstrap's readiness line has already been consumed; the reader starts at the
    /// command's first output byte.
    pub(crate) stdout: BufReader<ChildStdout>,
    pub(crate) stderr: ChildStderr,
}

impl WarmPwsh {
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

struct PoolState {
    ready: VecDeque<WarmPwsh>,
    warming: usize,
    closed: bool,
}

struct PoolInner {
    target: usize,
    state: Mutex<PoolState>,
    notify: Notify,
}

impl PoolInner {
    fn lock(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Starts replacements until `target` processes are ready or warming.
    fn ensure_filled(self: &Arc<Self>) {
        if self.target == 0 {
            return;
        }
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle,
            // Construction can happen outside a runtime (a plain `HarnessAgent::build()`).
            // The first async `execute` retries the fill.
            Err(_) => return,
        };
        let need = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            let need = self
                .target
                .saturating_sub(state.ready.len().saturating_add(state.warming));
            if need == 0 {
                return;
            }
            state.warming += need;
            need
        };
        for _ in 0..need {
            let inner = Arc::clone(self);
            handle.spawn(async move { inner.warm_one().await });
        }
    }

    async fn warm_one(self: Arc<Self>) {
        let started = spawn_warm().await;
        let mut state = self.lock();
        state.warming = state.warming.saturating_sub(1);
        if state.closed {
            drop(state);
            drop(started);
            return;
        }
        match started {
            Ok(warm) => state.ready.push_back(warm),
            Err(error) => {
                tracing::warn!(%error, "could not warm a pwsh process");
            }
        }
        drop(state);
        // Wake checkout waiters even on failure so they can fall back to the cold path now.
        self.notify.notify_waiters();
    }

    fn close(&self) {
        let ready = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            std::mem::take(&mut state.ready)
        };
        // Dropping a `Child` with `kill_on_drop(true)` terminates whatever was still idle.
        drop(ready);
        self.notify.notify_waiters();
    }
}

/// A per-session pool of pre-warmed one-shot `pwsh` processes.
///
/// The pool itself is cheap to hold; actual processes are started lazily/immediately depending
/// on whether construction happens inside a Tokio runtime. Dropping the last handle closes idle
/// processes and stops refills.
pub struct PwshProcessPool {
    inner: Arc<PoolInner>,
}

impl PwshProcessPool {
    /// Creates a pool that keeps up to `size` warmed processes.
    ///
    /// A size of zero disables pooling and leaves every call on the cold path.
    pub fn new(size: usize) -> Self {
        let inner = Arc::new(PoolInner {
            target: size,
            state: Mutex::new(PoolState {
                ready: VecDeque::new(),
                warming: 0,
                closed: false,
            }),
            notify: Notify::new(),
        });
        // If a runtime is already available (ACP builds sessions inside one), start warming
        // before the first command. Otherwise the first `execute` starts the fill.
        inner.ensure_filled();
        Self { inner }
    }

    /// Number of processes that have finished startup and are ready to take a command.
    pub fn ready_count(&self) -> usize {
        self.inner.lock().ready.len()
    }

    /// Number of processes currently starting.
    pub fn warming_count(&self) -> usize {
        self.inner.lock().warming
    }

    /// Takes one warm process, or `None` when the caller should use the cold path.
    ///
    /// A replacement is requested immediately, before the returned process runs, so its
    /// startup overlaps with the command instead of starting after it exits.
    pub(crate) async fn checkout(&self) -> Option<WarmPwsh> {
        if self.inner.target == 0 {
            return None;
        }
        self.inner.ensure_filled();
        let deadline = Instant::now() + CHECKOUT_WAIT;
        loop {
            // Scope the lock: an `if let` scrutinee temporary would stay alive for the whole
            // body, and `ensure_filled` locks the same state again.
            let ready = { self.inner.lock().ready.pop_front() };
            if let Some(mut warm) = ready {
                if warm.is_alive() {
                    self.inner.ensure_filled();
                    return Some(warm);
                }
                // The process died while idle (or was killed externally): discard it and ask
                // for a replacement.
                drop(warm);
                self.inner.ensure_filled();
                continue;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            tokio::select! {
                _ = self.inner.notify.notified() => {}
                _ = tokio::time::sleep(remaining) => return None,
            }
        }
    }

    /// Stops accepting work, discards idle processes and lets warming tasks die naturally.
    pub fn shutdown(&self) {
        self.inner.close();
    }
}

impl Drop for PwshProcessPool {
    fn drop(&mut self) {
        self.inner.close();
    }
}

async fn spawn_warm() -> Result<WarmPwsh, String> {
    let args = build_warm_bootstrap();
    let mut child = Command::new(&args[0])
        .args(&args[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("failed to spawn pwsh: {error}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "pwsh stdin was not piped".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "pwsh stdout was not piped".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "pwsh stderr was not piped".to_string())?;

    let mut stdout = BufReader::new(stdout);
    let mut line = String::new();
    match tokio::time::timeout(WARMUP_TIMEOUT, stdout.read_line(&mut line)).await {
        Ok(Ok(0)) | Err(_) => {
            let _ = child.kill().await;
            return Err("pwsh exited (or timed out) before becoming ready".to_string());
        }
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            let _ = child.kill().await;
            return Err(format!("could not read pwsh readiness: {error}"));
        }
    }
    if line.trim() != READY_MARKER {
        let _ = child.kill().await;
        return Err(format!(
            "pwsh startup produced an unexpected readiness line: {:?}",
            line.trim()
        ));
    }
    Ok(WarmPwsh {
        child,
        stdin,
        stdout,
        stderr,
    })
}

/// The bootstrap runs in every warmed process.
///
/// The status assignment is appended to the *user command* (before `Invoke-Expression` runs
/// it): `$?` read after `Invoke-Expression` itself would describe that call, not the failed
/// native command inside it. The explicit `exit 0/1` reproduces the cold path's exit code,
/// including the `cmd /c exit 7` → 1 normalization; an explicit `exit N`/`throw` inside the
/// user command exits before the wrapper's exit. Completion is signalled by process exit/EOF.
fn build_warm_bootstrap() -> Vec<String> {
    let script = format!(
        "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8\n\
         $OutputEncoding = [System.Text.Encoding]::UTF8\n\
         [Console]::Out.WriteLine('{READY_MARKER}')\n\
         [Console]::Out.Flush()\n\
         $__as_cwd = [Console]::In.ReadLine()\n\
         if ($null -ne $__as_cwd) {{\n\
           $__as_cmd = [Console]::In.ReadLine()\n\
           $__as_cwd = [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String($__as_cwd))\n\
           $__as_cmd = [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String($__as_cmd))\n\
           Set-Location -LiteralPath $__as_cwd\n\
           $__as_ok = $true\n\
           $__as_wrapped = $__as_cmd + [Environment]::NewLine + '$__as_ok = $?'\n\
           Invoke-Expression $__as_wrapped\n\
           if ($__as_ok) {{ exit 0 }} else {{ exit 1 }}\n\
         }}\n"
    );
    let utf16: Vec<u16> = script.encode_utf16().collect();
    let bytes: Vec<u8> = utf16.iter().flat_map(|word| word.to_le_bytes()).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    vec![
        "pwsh".to_string(),
        "-NoProfile".to_string(),
        "-NoLogo".to_string(),
        "-NonInteractive".to_string(),
        "-EncodedCommand".to_string(),
        encoded,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bootstrap_encodes_as_pwsh_expects() {
        let args = build_warm_bootstrap();
        assert_eq!(args[0], "pwsh");
        assert!(args.iter().any(|arg| arg == "-NoProfile"));
        let encoded = args.last().expect("encoded command");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("valid base64");
        let utf16: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let script = String::from_utf16(&utf16).expect("UTF-16 script");
        assert!(script.contains(READY_MARKER));
        assert!(script.contains("Invoke-Expression $__as_wrapped"));
        assert!(script.contains("$__as_ok = $?"));
    }
}
