//! Global graceful shutdown lifecycle and active request tracking, mirroring
//! `io.agentscope.core.shutdown`.
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::OnceLock;

use tokio::sync::RwLock;

use crate::error::Result;

/// Global framework shutdown state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownState {
    Running,
    ShuttingDown,
    Terminated,
}

/// Policy for handling incomplete reasoning content on shutdown interruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartialReasoningPolicy {
    Save,
    Discard,
}

/// Configuration for graceful shutdown behavior.
#[derive(Debug, Clone)]
pub struct GracefulShutdownConfig {
    /// Maximum wait for ongoing operations; `None` waits indefinitely.
    pub shutdown_timeout: Option<std::time::Duration>,
    pub partial_reasoning_policy: PartialReasoningPolicy,
}

impl Default for GracefulShutdownConfig {
    fn default() -> Self {
        Self {
            shutdown_timeout: None,
            partial_reasoning_policy: PartialReasoningPolicy::Save,
        }
    }
}

impl GracefulShutdownConfig {
    pub fn new(
        shutdown_timeout: Option<std::time::Duration>,
        partial_reasoning_policy: PartialReasoningPolicy,
    ) -> Result<Self> {
        if let Some(t) = shutdown_timeout {
            if t.is_zero() {
                return Err(crate::Error::Other("shutdownTimeout must be > 0 or None".into()));
            }
        }
        Ok(Self { shutdown_timeout, partial_reasoning_policy })
    }
}

const STATE_RUNNING: u8 = 0;
const STATE_SHUTTING_DOWN: u8 = 1;
const STATE_TERMINATED: u8 = 2;

/// Global manager for graceful shutdown lifecycle and active request tracking.
pub struct GracefulShutdownManager {
    state: AtomicU8,
    config: RwLock<GracefulShutdownConfig>,
    active_requests: AtomicUsize,
    shutdown_started: tokio::sync::Mutex<Option<std::time::Instant>>,
    terminated: tokio::sync::Notify,
}

impl Default for GracefulShutdownManager {
    fn default() -> Self {
        Self::new()
    }
}

impl GracefulShutdownManager {
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(STATE_RUNNING),
            config: RwLock::new(GracefulShutdownConfig::default()),
            active_requests: AtomicUsize::new(0),
            shutdown_started: tokio::sync::Mutex::new(None),
            terminated: tokio::sync::Notify::new(),
        }
    }

    /// The process-wide singleton instance.
    pub fn instance() -> &'static GracefulShutdownManager {
        static INSTANCE: OnceLock<GracefulShutdownManager> = OnceLock::new();
        INSTANCE.get_or_init(GracefulShutdownManager::new)
    }

    pub fn get_state(&self) -> ShutdownState {
        match self.state.load(Ordering::SeqCst) {
            STATE_SHUTTING_DOWN => ShutdownState::ShuttingDown,
            STATE_TERMINATED => ShutdownState::Terminated,
            _ => ShutdownState::Running,
        }
    }

    pub async fn get_config(&self) -> GracefulShutdownConfig {
        self.config.read().await.clone()
    }

    pub async fn set_config(&self, config: GracefulShutdownConfig) {
        *self.config.write().await = config;
    }

    pub fn is_accepting_requests(&self) -> bool {
        self.get_state() == ShutdownState::Running
    }

    pub fn get_active_request_count(&self) -> usize {
        self.active_requests.load(Ordering::SeqCst)
    }

    pub fn ensure_accepting_requests(&self) -> Result<()> {
        if !self.is_accepting_requests() {
            return Err(crate::Error::ShuttingDown("agent is shutting down".into()));
        }
        Ok(())
    }

    pub fn register_request(&self) {
        self.active_requests.fetch_add(1, Ordering::SeqCst);
    }

    pub fn unregister_request(&self) {
        self.active_requests.fetch_sub(1, Ordering::SeqCst);
        self.update_terminated_if_no_requests();
    }

    /// Initiate graceful shutdown. Returns `true` if the transition happened.
    pub async fn perform_graceful_shutdown(&self) -> bool {
        loop {
            let cur = self.state.load(Ordering::SeqCst);
            if cur == STATE_TERMINATED {
                return false;
            }
            if self.state.compare_exchange(cur, STATE_SHUTTING_DOWN, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                if cur == STATE_RUNNING {
                    *self.shutdown_started.lock().await = Some(std::time::Instant::now());
                }
                self.update_terminated_if_no_requests();
                return true;
            }
        }
    }

    fn update_terminated_if_no_requests(&self) {
        if self.get_state() == ShutdownState::ShuttingDown
            && self.active_requests.load(Ordering::SeqCst) == 0
            && self.state.swap(STATE_TERMINATED, Ordering::SeqCst) != STATE_TERMINATED
        {
            self.terminated.notify_waiters();
        }
    }

    /// Wait until TERMINATED or the given timeout elapses.
    pub async fn await_termination(&self, timeout: Option<std::time::Duration>) -> bool {
        let fut = self.terminated.notified();
        tokio::pin!(fut);
        if let Some(t) = timeout {
            tokio::time::timeout(t, fut).await.is_ok()
        } else {
            fut.await;
            true
        }
    }
}
