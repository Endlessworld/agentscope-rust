//! Fair per-key mutual exclusion for gateway turns, mirroring `SessionTurnGate`.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedMutexGuard, Mutex as TokioMutex};

/// Fair per-key mutual exclusion for gateway turns (user/channel inbound runs and
/// subagent notification).
pub struct SessionTurnGate {
    gates: Mutex<HashMap<String, Arc<TokioMutex<()>>>>,
}

impl SessionTurnGate {
    pub fn new() -> Self {
        Self {
            gates: Mutex::new(HashMap::new()),
        }
    }

    /// Acquires the gate for `key`. The returned guard releases the gate on drop,
    /// so concurrent turns for the same key serialize (fair FIFO).
    pub async fn acquire(&self, key: &str) -> OwnedMutexGuard<()> {
        let m = self
            .gates
            .lock()
            .unwrap()
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(TokioMutex::new(())))
            .clone();
        m.lock_owned().await
    }

    /// Non-blocking check: `true` when a turn is currently held for `key` (i.e. a
    /// run is in progress). Used to skip sessions that are already active.
    pub async fn is_running(&self, key: &str) -> bool {
        let m = self.gates.lock().unwrap().get(key).cloned();
        match m {
            Some(m) => m.try_lock().is_err(),
            None => false,
        }
    }
}

impl Default for SessionTurnGate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serializes_concurrent_turns_for_same_key() {
        let gate = SessionTurnGate::new();
        let key = "session-1";
        let guard = gate.acquire(key).await;
        // While held, the gate reports running.
        assert!(gate.is_running(key).await);
        drop(guard);
        assert!(!gate.is_running(key).await);
    }

    #[tokio::test]
    async fn different_keys_do_not_block_each_other() {
        let gate = SessionTurnGate::new();
        let _g1 = gate.acquire("a").await;
        // Key "b" is not running even while "a" is held.
        assert!(gate.is_running("a").await);
        assert!(!gate.is_running("b").await);
    }

    #[tokio::test]
    async fn unknown_key_reports_not_running() {
        let gate = SessionTurnGate::new();
        assert!(!gate.is_running("nope").await);
    }
}
