//! Per-process dispatcher that wakes idle sessions when background work completes,
//! mirroring `WakeupDispatcher`.
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use agentscope_core::error::Result;
use agentscope_core::message::Msg;
use serde_json::Value;

use crate::agent::bus::{MessageBus, WAKEUPS_KEY};

/// Max entries drained from the wakeup queue per signal.
const MAX_DRAIN_COUNT: usize = 64;

/// Thin interface for the two gateway operations the dispatcher needs.
#[async_trait]
pub trait WakeupTarget: Send + Sync {
    async fn is_session_running(&self, session_id: &str) -> bool;
    /// Triggers a wakeup run for an idle session. Returns `Ok(None)` for an unknown
    /// session.
    async fn run_wakeup(&self, user_id: Option<&str>, session_id: &str) -> Result<Option<Msg>>;
}

/// State shared with the spawned background task (both halves owned via `Arc`).
struct DispatcherCore {
    message_bus: Arc<dyn MessageBus>,
    target: Arc<dyn WakeupTarget>,
}

impl DispatcherCore {
    async fn listen_loop(&self) {
        let mut rx = self.message_bus.subscribe_wakeup();
        while rx.recv().await.is_some() {
            self.drain_and_dispatch().await;
        }
    }

    async fn drain_and_dispatch(&self) {
        let Ok(entries) = self.message_bus.queue_drain(WAKEUPS_KEY, MAX_DRAIN_COUNT).await else {
            return;
        };
        if entries.is_empty() {
            return;
        }
        for entry in entries {
            self.dispatch(&entry.payload).await;
        }
    }

    async fn dispatch(&self, payload: &Value) {
        let user_id = payload.get("userId").and_then(Value::as_str).map(str::to_string);
        let session_id = payload.get("sessionId").and_then(Value::as_str);
        let Some(session_id) = session_id else {
            return;
        };
        if session_id.trim().is_empty() {
            return;
        }
        if self.target.is_session_running(session_id).await {
            // Current run will drain its inbox naturally.
            return;
        }
        if let Err(e) = self.target.run_wakeup(user_id.as_deref(), session_id).await {
            tracing::warn!("WakeupDispatcher: wakeup run failed for session {session_id}: {e}");
        }
    }
}

/// Dispatches queued wakeups to idle sessions on the shared signal channel.
pub struct WakeupDispatcher {
    core: Arc<DispatcherCore>,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl WakeupDispatcher {
    pub fn new(message_bus: Arc<dyn MessageBus>, target: Arc<dyn WakeupTarget>) -> Self {
        Self {
            core: Arc::new(DispatcherCore { message_bus, target }),
            handle: Mutex::new(None),
        }
    }

    /// Starts the dispatcher: performs an initial drain of any queued wakeups, then
    /// subscribes to the live signal channel.
    pub fn start(&self) {
        let core = self.core.clone();
        let handle = tokio::spawn(async move {
            core.drain_and_dispatch().await;
            core.listen_loop().await;
        });
        *self.handle.lock().unwrap() = Some(handle);
    }

    /// Stops the dispatcher by aborting the background task.
    pub fn close(&self) {
        if let Some(h) = self.handle.lock().unwrap().take() {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::bus::WorkspaceMessageBus;
    use crate::filesystem::{Filesystem, LocalFilesystem};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StubTarget {
        wakes: AtomicUsize,
        running: Vec<String>,
    }

    #[async_trait]
    impl WakeupTarget for StubTarget {
        async fn is_session_running(&self, session_id: &str) -> bool {
            self.running.contains(&session_id.to_string())
        }
        async fn run_wakeup(&self, _user_id: Option<&str>, _session_id: &str) -> Result<Option<Msg>> {
            self.wakes.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }
    }

    #[tokio::test]
    async fn dispatches_queued_wakeups_and_skips_running_sessions() {
        let dir = std::env::temp_dir().join(format!("wakeup_test_{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
        let bus: Arc<dyn MessageBus> = Arc::new(WorkspaceMessageBus::new(fs, "/bus"));
        let target = Arc::new(StubTarget {
            wakes: AtomicUsize::new(0),
            running: vec!["running-sess".to_string()],
        });

        bus.enqueue_wakeup("alice", "sess-1", "main").await.unwrap();
        bus.enqueue_wakeup("bob", "running-sess", "main").await.unwrap();

        let dispatcher = WakeupDispatcher::new(bus, target.clone());
        // Initial drain runs synchronously; invoke the core directly (not the spawned task).
        dispatcher.core.drain_and_dispatch().await;
        assert_eq!(target.wakes.load(Ordering::SeqCst), 1); // sess-1 only

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
