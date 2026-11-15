//! A scripted sandbox: no process, no clock, no I/O.
//!
//! Every spawn takes the next script from a queue and each poll advances that script by one step;
//! once the script is exhausted its last answer repeats, so "still running" is written as a single
//! [`Poll::running`]. A killed run always answers `Exited { code: kill_exit }`, which is what a
//! real backend reports when the process was stopped rather than finished.
//!
//! This is production code, not a test helper: a crate that wires a durable turn against a sandbox
//! needs a way to prove the wiring without a container, and the scripted answers are exactly the
//! ones that are hard to produce on demand from a real sandbox.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::sandbox::{ExecSpec, Handle, Poll, Sandbox, SandboxError};

/// Exit code a killed run reports unless told otherwise (128 + SIGKILL).
const DEFAULT_KILL_EXIT: i32 = 137;

struct Run {
    script: Vec<Poll>,
    cursor: usize,
    killed: bool,
}

struct Inner {
    scripts: VecDeque<Vec<Poll>>,
    fallback: Poll,
    kill_exit: i32,
    kill_stderr: Vec<u8>,
    runs: HashMap<String, Run>,
    spawned: Vec<ExecSpec>,
    kills: Vec<String>,
    sequence: u64,
}

/// A [`Sandbox`] that answers from scripts.
pub struct FakeSandbox {
    inner: Mutex<Inner>,
}

impl FakeSandbox {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                scripts: VecDeque::new(),
                fallback: Poll::exited(0, "", ""),
                kill_exit: DEFAULT_KILL_EXIT,
                kill_stderr: b"killed".to_vec(),
                runs: HashMap::new(),
                spawned: Vec::new(),
                kills: Vec::new(),
                sequence: 0,
            }),
        }
    }

    /// Queues the answers for the next spawn, in poll order.
    pub fn script(self, polls: Vec<Poll>) -> Self {
        self.lock().scripts.push_back(polls);
        self
    }

    /// What a spawn answers when the queue is empty.
    pub fn fallback(self, poll: Poll) -> Self {
        self.lock().fallback = poll;
        self
    }

    /// Exit code a killed run reports.
    pub fn kill_exit(self, code: i32) -> Self {
        self.lock().kill_exit = code;
        self
    }

    /// Every spec the sandbox was asked to run, in spawn order. Nothing is rewritten, so a test
    /// can compare it byte for byte with what the call carried.
    pub fn spawned_specs(&self) -> Vec<ExecSpec> {
        self.lock().spawned.clone()
    }

    /// Handles that received a kill, in order.
    pub fn kills(&self) -> Vec<String> {
        self.lock().kills.clone()
    }

    pub fn run_count(&self) -> usize {
        self.lock().runs.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("fake sandbox lock")
    }
}

impl Default for FakeSandbox {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Sandbox for FakeSandbox {
    async fn spawn(&self, spec: ExecSpec) -> Result<Handle, SandboxError> {
        if spec.command.trim().is_empty() {
            return Err(SandboxError::invalid("`command` must not be blank"));
        }
        let mut inner = self.lock();
        inner.sequence += 1;
        let id = format!("fake-{}", inner.sequence);
        let script = inner
            .scripts
            .pop_front()
            .unwrap_or_else(|| vec![inner.fallback.clone()]);
        inner.spawned.push(spec);
        inner.runs.insert(
            id.clone(),
            Run {
                script,
                cursor: 0,
                killed: false,
            },
        );
        Ok(Handle::new(id))
    }

    async fn poll(&self, handle: &Handle) -> Result<Poll, SandboxError> {
        self.advance(handle)
    }

    async fn kill(&self, handle: &Handle) -> Result<(), SandboxError> {
        let mut inner = self.lock();
        let run = inner.runs.get_mut(&handle.id).ok_or_else(|| {
            SandboxError::not_found(format!("no such sandbox handle: {}", handle.id))
        })?;
        run.killed = true;
        inner.kills.push(handle.id.clone());
        Ok(())
    }
}

impl FakeSandbox {
    /// Answers one poll and advances the run's cursor.
    fn advance(&self, handle: &Handle) -> Result<Poll, SandboxError> {
        let mut inner = self.lock();
        let (kill_exit, kill_stderr) = (inner.kill_exit, inner.kill_stderr.clone());
        let run = inner.runs.get_mut(&handle.id).ok_or_else(|| {
            SandboxError::not_found(format!("no such sandbox handle: {}", handle.id))
        })?;
        if run.killed {
            return Ok(Poll::Exited {
                code: kill_exit,
                stdout: Vec::new(),
                stderr: kill_stderr,
                truncated: false,
            });
        }
        let step = run.cursor.min(run.script.len().saturating_sub(1));
        run.cursor = (run.cursor + 1).min(run.script.len().saturating_sub(1));
        Ok(run.script[step].clone())
    }
}
