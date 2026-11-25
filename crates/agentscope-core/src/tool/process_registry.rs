//! Session-scoped registry of running command processes.
//!
//! ACP v2 exposes agent-owned terminals: the agent starts a command, reports it as a
//! terminal, and lets the client cancel it. The kill primitive itself lives in the
//! platform layer (the sandbox owns the child process), so this registry only keeps a
//! keyed callback per running command plus the cancellation flag the tool reads back
//! when it finishes. Tools that never register anything are unaffected.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Runtime-context key carrying the session's [`ProcessRegistry`].
pub const PROCESS_REGISTRY_CONTEXT_KEY: &str = "process_registry";

type KillFn = Arc<dyn Fn() + Send + Sync>;

/// Lifecycle and output events of a running command, forwarded by the protocol layer.
///
/// Every method has a no-op default: a host that only needs cancellation implements none of
/// them, and one that installs nothing (`v1`) pays nothing.
pub trait ProcessEvents: Send + Sync {
    /// The command started; `cwd` is the absolute directory it runs in.
    fn started(&self, key: &str, command: &str, cwd: &str) {
        let _ = (key, command, cwd);
    }

    /// One chunk of combined output, forwarded as it is read.
    fn output(&self, key: &str, bytes: &[u8]) {
        let _ = (key, bytes);
    }

    /// The command exited (or was killed) with `exit_code`.
    fn exited(&self, key: &str, exit_code: i32) {
        let _ = (key, exit_code);
    }
}

struct Entry {
    kill: KillFn,
    cancelled: Arc<AtomicBool>,
}

/// Processes started by this session, keyed by the tool call that owns them.
#[derive(Default)]
pub struct ProcessRegistry {
    entries: Mutex<HashMap<String, Entry>>,
    events: Mutex<Option<Arc<dyn ProcessEvents>>> ,
}

impl ProcessRegistry {
    /// Creates an empty registry.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn entries(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Installs the event sink (the protocol layer sets it; v1 leaves it unset).
    pub fn set_events(&self, events: Option<Arc<dyn ProcessEvents>>) {
        *self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = events;
    }

    fn event_sink(&self) -> Option<Arc<dyn ProcessEvents>> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Reports that a command started under `key`.
    pub fn emit_started(&self, key: &str, command: &str, cwd: &str) {
        if let Some(events) = self.event_sink() {
            events.started(key, command, cwd);
        }
    }

    /// Forwards one output chunk; a no-op without an event sink.
    pub fn emit_output(&self, key: &str, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Some(events) = self.event_sink() {
            events.output(key, bytes);
        }
    }

    /// Reports that a command under `key` exited.
    pub fn emit_exited(&self, key: &str, exit_code: i32) {
        if let Some(events) = self.event_sink() {
            events.exited(key, exit_code);
        }
    }

    /// Registers a running process; dropping the returned guard unregisters it.
    pub fn register(
        self: &Arc<Self>,
        key: impl Into<String>,
        kill: impl Fn() + Send + Sync + 'static,
    ) -> ProcessGuard {
        let key = key.into();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.entries().insert(
            key.clone(),
            Entry {
                kill: Arc::new(kill),
                cancelled: Arc::clone(&cancelled),
            },
        );
        ProcessGuard {
            registry: Arc::clone(self),
            key,
            cancelled,
        }
    }

    /// Cancels the process registered under `key`: marks it cancelled, then kills it.
    ///
    /// Returns false when no process is registered (unknown key, or already finished).
    pub fn cancel(&self, key: &str) -> bool {
        let kill = {
            let mut entries = self.entries();
            match entries.get_mut(key) {
                Some(entry) => {
                    entry.cancelled.store(true, Ordering::SeqCst);
                    Some(Arc::clone(&entry.kill))
                }
                None => None,
            }
        };
        match kill {
            Some(kill) => {
                kill();
                true
            }
            None => false,
        }
    }

    /// Whether a process is currently registered under `key`.
    pub fn is_running(&self, key: &str) -> bool {
        self.entries().contains_key(key)
    }
}

/// Keeps one process registered until the guard is dropped (the tool returned, panicked,
/// or the turn was interrupted).
pub struct ProcessGuard {
    registry: Arc<ProcessRegistry>,
    key: String,
    cancelled: Arc<AtomicBool>,
}

impl ProcessGuard {
    /// Whether the client cancelled this process while it was running.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        self.registry.entries().remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_marks_and_kills_the_registered_process() {
        let registry = ProcessRegistry::new();
        let killed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&killed);
        let guard = registry.register("call-1", move || flag.store(true, Ordering::SeqCst));

        assert!(registry.is_running("call-1"));
        assert!(!guard.is_cancelled());
        assert!(registry.cancel("call-1"));
        assert!(killed.load(Ordering::SeqCst));
        assert!(guard.is_cancelled());
        assert!(registry.is_running("call-1"));

        drop(guard);
        assert!(!registry.is_running("call-1"));
        assert!(!registry.cancel("call-1"));
    }

    struct RecordingEvents {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl ProcessEvents for RecordingEvents {
        fn started(&self, key: &str, command: &str, cwd: &str) {
            self.seen
                .lock()
                .unwrap()
                .push(format!("started {key} {command} {cwd}"));
        }

        fn output(&self, key: &str, bytes: &[u8]) {
            self.seen.lock().unwrap().push(format!(
                "output {key} {}",
                String::from_utf8_lossy(bytes)
            ));
        }

        fn exited(&self, key: &str, exit_code: i32) {
            self.seen
                .lock()
                .unwrap()
                .push(format!("exited {key} {exit_code}"));
        }
    }

    #[test]
    fn lifecycle_and_output_events_reach_the_installed_sink() {
        let registry = ProcessRegistry::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        registry.set_events(Some(Arc::new(RecordingEvents {
            seen: Arc::clone(&seen),
        })));

        registry.emit_started("call-1", "ls", "D:/tmp");
        registry.emit_output("call-1", b"hello");
        registry.emit_output("call-1", b"");
        registry.emit_exited("call-1", 0);
        registry.set_events(None);
        registry.emit_output("call-1", b"ignored");

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.as_slice(),
            [
                "started call-1 ls D:/tmp",
                "output call-1 hello",
                "exited call-1 0"
            ]
        );
    }
}
