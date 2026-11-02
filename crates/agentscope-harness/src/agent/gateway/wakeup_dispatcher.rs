//! Per-process dispatcher that wakes idle sessions when background work completes,
//! mirroring `WakeupDispatcher`.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agentscope_core::error::Result;
use agentscope_core::message::Msg;
use async_trait::async_trait;
use serde_json::Value;

use crate::agent::bus::{MessageBus, WAKEUPS_KEY};

/// Max entries drained from the wakeup queue per signal.
const MAX_DRAIN_COUNT: usize = 64;

/// How often a deferred wakeup is re-checked.
///
/// The bus signal alone is enough with the filesystem bus (it ticks on its own), but a bus
/// that only signals on publish would leave a deferred wakeup waiting forever.
const DEFERRED_RETRY: std::time::Duration = std::time::Duration::from_secs(3);

/// Thin interface for the two gateway operations the dispatcher needs.
#[async_trait]
pub trait WakeupTarget: Send + Sync {
    async fn is_session_running(&self, session_id: &str) -> bool;
    /// Whether the session still holds something the model has not seen: an undelivered
    /// task result or an inbox hint.
    ///
    /// A wakeup that lands while a turn is running is retried once that turn ends. When the
    /// turn read the result before ending, this reports `false` and no empty turn is
    /// started. Defaults to `true`: a target that cannot tell must not swallow a wakeup.
    async fn has_pending_work(&self, _session_id: &str) -> bool {
        true
    }
    /// Triggers a wakeup run for an idle session. Returns `Ok(None)` for an unknown
    /// session.
    async fn run_wakeup(&self, user_id: Option<&str>, session_id: &str) -> Result<Option<Msg>>;
}

/// State shared with the spawned background task (both halves owned via `Arc`).
struct DispatcherCore {
    message_bus: Arc<dyn MessageBus>,
    target: Arc<dyn WakeupTarget>,
    /// Wakeups that arrived while their session was running, keyed by session id.
    ///
    /// The running turn only sees the result if it reaches another reasoning step, so a turn
    /// that is already wrapping up would leave the wakeup unserved. These are retried once
    /// the turn ends instead of being dropped; a session that is still busy keeps its entry.
    deferred: Mutex<HashMap<String, Option<String>>>,
}

impl DispatcherCore {
    async fn listen_loop(&self) {
        let mut rx = self.message_bus.subscribe_wakeup();
        let mut retry = tokio::time::interval(DEFERRED_RETRY);
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                received = rx.recv() => {
                    if received.is_none() {
                        return;
                    }
                    self.drain_and_dispatch().await;
                }
                _ = retry.tick() => {}
            }
            self.dispatch_deferred().await;
        }
    }

    /// Moves every queued wakeup into the deferred set, then tries to serve them all.
    async fn drain_and_dispatch(&self) {
        let Ok(entries) = self
            .message_bus
            .queue_drain(WAKEUPS_KEY, MAX_DRAIN_COUNT)
            .await
        else {
            return;
        };
        {
            let mut deferred = self
                .deferred
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for entry in entries {
                let payload = entry.payload;
                let Some(session_id) = payload.get("sessionId").and_then(Value::as_str) else {
                    continue;
                };
                if session_id.trim().is_empty() {
                    continue;
                }
                let user_id = payload
                    .get("userId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                deferred.insert(session_id.to_string(), user_id);
            }
        }
        self.dispatch_deferred().await;
    }

    /// Serves the deferred wakeups whose session went idle, keeping the ones that did not.
    async fn dispatch_deferred(&self) {
        let waiting: Vec<(String, Option<String>)> = {
            let mut deferred = self
                .deferred
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if deferred.is_empty() {
                return;
            }
            deferred.drain().collect()
        };
        for (session_id, user_id) in waiting {
            if self.target.is_session_running(&session_id).await {
                // Still busy: the turn may yet reach a reasoning step that reads the result.
                self.defer(session_id, user_id);
                continue;
            }
            // Nothing left to deliver means the turn read it before ending; starting an
            // empty turn would only make the model re-answer the previous question.
            if !self.target.has_pending_work(&session_id).await {
                continue;
            }
            // One attempt per completion: a failing turn is not retried in a loop.
            if let Err(e) = self
                .target
                .run_wakeup(user_id.as_deref(), &session_id)
                .await
            {
                tracing::warn!("WakeupDispatcher: wakeup run failed for session {session_id}: {e}");
            }
        }
    }

    fn defer(&self, session_id: String, user_id: Option<String>) {
        self.deferred
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id, user_id);
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
            core: Arc::new(DispatcherCore {
                message_bus,
                target,
                deferred: Mutex::new(HashMap::new()),
            }),
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct StubTarget {
        wakes: AtomicUsize,
        /// Sessions that currently have a turn running.
        running: Mutex<Vec<String>>,
        /// Whether the session still holds something to deliver.
        pending: AtomicBool,
    }

    impl StubTarget {
        fn new(running: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                wakes: AtomicUsize::new(0),
                running: Mutex::new(running.iter().map(|s| s.to_string()).collect()),
                pending: AtomicBool::new(true),
            })
        }

        fn finish_turns(&self) {
            self.running.lock().unwrap().clear();
        }

        fn wakes(&self) -> usize {
            self.wakes.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl WakeupTarget for StubTarget {
        async fn is_session_running(&self, session_id: &str) -> bool {
            self.running.lock().unwrap().iter().any(|s| s == session_id)
        }
        async fn has_pending_work(&self, _session_id: &str) -> bool {
            self.pending.load(Ordering::SeqCst)
        }
        async fn run_wakeup(
            &self,
            _user_id: Option<&str>,
            _session_id: &str,
        ) -> Result<Option<Msg>> {
            self.wakes.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }
    }

    async fn dispatcher_with(dir_tag: &str, target: Arc<StubTarget>) -> WakeupDispatcher {
        let dir =
            std::env::temp_dir().join(format!("wakeup_test_{dir_tag}_{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let fs: Arc<dyn Filesystem> = Arc::new(LocalFilesystem::sandboxed(&dir));
        let bus: Arc<dyn MessageBus> = Arc::new(WorkspaceMessageBus::new(fs, "/bus"));
        WakeupDispatcher::new(bus, target)
    }

    fn deferred_sessions(dispatcher: &WakeupDispatcher) -> Vec<String> {
        dispatcher
            .core
            .deferred
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn dispatches_queued_wakeups_and_defers_running_sessions() {
        let target = StubTarget::new(&["running-sess"]);
        let dispatcher = dispatcher_with("dispatch", target.clone()).await;
        let bus = dispatcher.core.message_bus.clone();

        bus.enqueue_wakeup("alice", "sess-1", "main").await.unwrap();
        bus.enqueue_wakeup("bob", "running-sess", "main")
            .await
            .unwrap();

        // Initial drain runs synchronously; invoke the core directly (not the spawned task).
        dispatcher.core.drain_and_dispatch().await;
        assert_eq!(target.wakes(), 1, "only the idle session may be woken");
        assert_eq!(
            deferred_sessions(&dispatcher),
            vec!["running-sess".to_string()],
            "a wakeup for a running session is kept, not dropped"
        );
    }

    #[tokio::test]
    async fn a_deferred_wakeup_is_served_once_when_the_turn_ends() {
        let target = StubTarget::new(&["busy"]);
        let dispatcher = dispatcher_with("deferred", target.clone()).await;
        let bus = dispatcher.core.message_bus.clone();

        bus.enqueue_wakeup("bob", "busy", "main").await.unwrap();
        dispatcher.core.drain_and_dispatch().await;
        assert_eq!(target.wakes(), 0);
        assert_eq!(deferred_sessions(&dispatcher), vec!["busy".to_string()]);

        // Still running: the entry survives another round of retries.
        dispatcher.core.dispatch_deferred().await;
        assert_eq!(target.wakes(), 0);
        assert_eq!(deferred_sessions(&dispatcher), vec!["busy".to_string()]);

        target.finish_turns();
        dispatcher.core.dispatch_deferred().await;
        assert_eq!(target.wakes(), 1, "the turn ended, so the wakeup runs");
        assert!(
            deferred_sessions(&dispatcher).is_empty(),
            "one attempt per completion: the entry is not retried again"
        );

        dispatcher.core.dispatch_deferred().await;
        assert_eq!(target.wakes(), 1, "and it is not replayed");
    }

    #[tokio::test]
    async fn a_deferred_wakeup_is_dropped_when_the_turn_read_the_result() {
        // The turn reached a reasoning step and drained the inbox before ending, so waking
        // the session again would only make the model re-answer the previous question.
        let target = StubTarget::new(&["busy"]);
        let dispatcher = dispatcher_with("read", target.clone()).await;
        let bus = dispatcher.core.message_bus.clone();

        bus.enqueue_wakeup("bob", "busy", "main").await.unwrap();
        dispatcher.core.drain_and_dispatch().await;
        target.finish_turns();
        target.pending.store(false, Ordering::SeqCst);

        dispatcher.core.dispatch_deferred().await;
        assert_eq!(target.wakes(), 0, "an empty wakeup turn must not start");
        assert!(deferred_sessions(&dispatcher).is_empty());
    }
}
