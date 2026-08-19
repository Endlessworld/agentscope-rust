//! Agent lifecycle infrastructure: per-key call serialization and graceful-shutdown
//! execution guards, mirroring the `AgentBase` lifecycle (callGates + acquire/release).

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::error::Result;
use crate::shutdown::GracefulShutdownManager;

/// Per-key FIFO serialization gate. Calls sharing the same key run one-at-a-time
/// in FIFO order, while calls with different keys run concurrently.
pub struct CallGate {
    tails: Mutex<HashMap<String, Arc<Notify>>>,
}

impl Default for CallGate {
    fn default() -> Self {
        Self::new()
    }
}

impl CallGate {
    /// Creates a new empty gate.
    pub fn new() -> Self {
        Self { tails: Mutex::new(HashMap::new()) }
    }

    /// Runs `fut`, serializing against other calls sharing the same `key`.
    pub async fn run<T, F>(&self, key: &str, fut: F) -> T
    where
        F: Future<Output = T>,
    {
        let release = Arc::new(Notify::new());
        let prev = {
            let mut map = self.tails.lock().unwrap();
            map.insert(key.to_string(), Arc::clone(&release))
        };
        if let Some(prev) = prev {
            prev.notified().await;
        }
        let _guard = ReleaseGuard { gate: self, key, release: Arc::clone(&release) };
        fut.await
    }
}

struct ReleaseGuard<'a> {
    gate: &'a CallGate,
    key: &'a str,
    release: Arc<Notify>,
}

impl Drop for ReleaseGuard<'_> {
    fn drop(&mut self) {
        {
            let mut map = self.gate.tails.lock().unwrap();
            if let Some(cur) = map.get(self.key) {
                if Arc::ptr_eq(cur, &self.release) {
                    map.remove(self.key);
                }
            }
        }
        self.release.notify_waiters();
    }
}

/// RAII guard that acquires execution on the graceful-shutdown manager and
/// releases it on drop, mirroring `AgentBase#acquireExecution`/`releaseExecution`.
pub struct ExecutionGuard;

impl ExecutionGuard {
    /// Ensures the framework is still accepting requests, then registers this call.
    pub fn acquire() -> Result<ExecutionGuard> {
        let mgr = GracefulShutdownManager::instance();
        mgr.ensure_accepting_requests()?;
        mgr.register_request();
        Ok(ExecutionGuard)
    }
}

impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        GracefulShutdownManager::instance().unregister_request();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn same_key_serializes_fifo() {
        let gate = Arc::new(CallGate::new());
        let order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for i in 0..3usize {
            let gate = Arc::clone(&gate);
            let order = Arc::clone(&order);
            handles.push(tokio::spawn(async move {
                gate.run("sess", async move {
                    order.lock().unwrap().push(format!("start{i}"));
                    tokio::time::sleep(Duration::from_millis(15)).await;
                    order.lock().unwrap().push(format!("end{i}"));
                })
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let order = order.lock().unwrap().clone();
        for i in 0..3usize {
            let si = order.iter().position(|x| x == &format!("start{i}")).unwrap();
            let ei = order.iter().position(|x| x == &format!("end{i}")).unwrap();
            assert!(si < ei, "start must precede end");
            if i > 0 {
                let prev_e = order.iter().position(|x| x == &format!("end{}", i - 1)).unwrap();
                assert!(prev_e < si, "previous call must finish before next starts");
            }
        }
    }

    #[tokio::test]
    async fn different_keys_run_concurrently() {
        let gate = Arc::new(CallGate::new());
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for i in 0..3usize {
            let gate = Arc::clone(&gate);
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            handles.push(tokio::spawn(async move {
                gate.run(&format!("k{i}"), async move {
                    let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(cur, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert!(max_active.load(Ordering::SeqCst) >= 2, "different keys must overlap");
    }

    #[tokio::test]
    async fn execution_guard_registers_and_releases() {
        let mgr = GracefulShutdownManager::instance();
        let before = mgr.get_active_request_count();
        {
            let guard = ExecutionGuard::acquire().unwrap();
            assert_eq!(mgr.get_active_request_count(), before + 1);
            drop(guard);
        }
        assert_eq!(mgr.get_active_request_count(), before);
    }
}
