//! Fair per-key mutual exclusion for gateway turns, mirroring `SessionTurnGate`
//! and `LocalSessionTurnGate`.
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as TokioMutex, OwnedMutexGuard};

/// Thrown when a session turn gate cannot be acquired because another turn holds it.
///
/// Mirrors `io.agentscope.harness.agent.gateway.TurnBusyException`: the gateway maps
/// this to a skipped (empty) result so duplicate wakeups / concurrent inbound requests
/// for the same session are dropped rather than queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnBusyException {
    gate_key: String,
}

impl TurnBusyException {
    pub fn new(gate_key: impl Into<String>) -> Self {
        Self {
            gate_key: gate_key.into(),
        }
    }

    /// The canonical session gate key that is busy.
    pub fn gate_key(&self) -> &str {
        &self.gate_key
    }
}

impl std::fmt::Display for TurnBusyException {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Session turn gate busy: {}", self.gate_key)
    }
}

impl std::error::Error for TurnBusyException {}

/// One mutex plus the number of leases currently holding or waiting on it.
struct Gate {
    mutex: Arc<TokioMutex<()>>,
    /// Holders plus waiters; mutated only while holding the map lock.
    leases: AtomicUsize,
}

/// A held gate lease. Dropping it releases the mutex and, once no holder or waiter
/// is left for the key, evicts the key from the gate map.
pub struct SessionTurnLease {
    _ticket: LeaseTicket,
    /// Taken first on drop so the permit is free before the key is retired.
    guard: Option<OwnedMutexGuard<()>>,
}

impl std::fmt::Debug for SessionTurnLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionTurnLease")
    }
}

impl Drop for SessionTurnLease {
    fn drop(&mut self) {
        drop(self.guard.take());
    }
}

/// Registration of one holder-or-waiter. Dropping it decrements the lease count and
/// evicts the key once the count reaches zero, so a cancelled `acquire` (future
/// dropped while waiting) cannot leak a lease.
struct LeaseTicket {
    gates: Arc<Mutex<HashMap<String, Arc<Gate>>>>,
    key: String,
    gate: Arc<Gate>,
}

impl Drop for LeaseTicket {
    fn drop(&mut self) {
        // The decrement and the removal run inside the map lock, so they are atomic
        // with respect to a concurrent `enter` on the same key: an acquirer either
        // joins the existing gate or, once it is gone, builds a fresh one.
        let mut gates = self
            .gates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let still_registered = gates
            .get(&self.key)
            .is_some_and(|existing| Arc::ptr_eq(existing, &self.gate));
        if !still_registered {
            return;
        }
        if self.gate.leases.fetch_sub(1, Ordering::SeqCst) == 1 {
            gates.remove(&self.key);
        }
    }
}

/// Fair per-key mutual exclusion for gateway turns (user/channel inbound runs and
/// subagent notification).
pub struct SessionTurnGate {
    gates: Arc<Mutex<HashMap<String, Arc<Gate>>>>,
}

impl SessionTurnGate {
    pub fn new() -> Self {
        Self {
            gates: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Registers one holder-or-waiter for `key`, creating the gate when absent.
    fn enter(&self, key: &str) -> LeaseTicket {
        let mut gates = self
            .gates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let gate = match gates.get(key).cloned() {
            Some(existing) => existing,
            None => {
                let gate = Arc::new(Gate {
                    mutex: Arc::new(TokioMutex::new(())),
                    leases: AtomicUsize::new(0),
                });
                gates.insert(key.to_string(), Arc::clone(&gate));
                gate
            }
        };
        gate.leases.fetch_add(1, Ordering::SeqCst);
        LeaseTicket {
            gates: Arc::clone(&self.gates),
            key: key.to_string(),
            gate,
        }
    }

    /// Acquires the gate for `key`. The returned lease releases the gate on drop,
    /// so concurrent turns for the same key serialize (fair FIFO). The key is
    /// evicted from the map once its last holder or waiter is gone.
    pub async fn acquire(&self, key: &str) -> SessionTurnLease {
        let ticket = self.enter(key);
        let mutex = Arc::clone(&ticket.gate.mutex);
        let guard = mutex.lock_owned().await;
        SessionTurnLease {
            _ticket: ticket,
            guard: Some(guard),
        }
    }

    /// Non-blocking acquire: returns [`TurnBusyException`] when another turn holds
    /// the gate for `key`.
    pub fn try_acquire(&self, key: &str) -> Result<SessionTurnLease, TurnBusyException> {
        let ticket = self.enter(key);
        match Arc::clone(&ticket.gate.mutex).try_lock_owned() {
            Ok(guard) => Ok(SessionTurnLease {
                _ticket: ticket,
                guard: Some(guard),
            }),
            Err(_) => Err(TurnBusyException::new(key)),
        }
    }

    /// Non-blocking check: `true` when a turn is currently held for `key` (i.e. a
    /// run is in progress). Used to skip sessions that are already active.
    pub async fn is_running(&self, key: &str) -> bool {
        let gate = self
            .gates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .cloned();
        match gate {
            Some(gate) => gate.mutex.try_lock().is_err(),
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

    fn lease_count(gate: &SessionTurnGate, key: &str) -> usize {
        gate.gates
            .lock()
            .unwrap()
            .get(key)
            .map(|g| g.leases.load(std::sync::atomic::Ordering::SeqCst))
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn idle_keys_are_evicted_from_the_gate_map() {
        let gate = SessionTurnGate::new();
        for i in 0..50 {
            let key = format!("session-{i}");
            drop(gate.acquire(&key).await);
        }
        assert!(gate.gates.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_key_is_evicted_only_after_its_last_waiter_leaves() {
        let gate = Arc::new(SessionTurnGate::new());
        let key = "session-waiter";
        let holder = gate.acquire(key).await;

        let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let waiter = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                let lease = gate.acquire(key).await;
                acquired_tx.send(()).unwrap();
                release_rx.await.unwrap();
                drop(lease);
            })
        };

        // Let the waiter register its lease before the holder leaves.
        while lease_count(&gate, key) < 2 {
            tokio::task::yield_now().await;
        }

        drop(holder);
        acquired_rx.await.unwrap();
        assert_eq!(lease_count(&gate, key), 1);
        assert!(gate.gates.lock().unwrap().contains_key(key));

        release_tx.send(()).unwrap();
        waiter.await.unwrap();
        assert!(gate.gates.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn try_acquire_errors_when_busy() {
        let gate = SessionTurnGate::new();
        let guard = gate.try_acquire("k").unwrap();
        let busy = gate.try_acquire("k").unwrap_err();
        assert_eq!(busy.gate_key(), "k");
        assert!(busy.to_string().contains("busy"));
        drop(guard);
        assert!(gate.try_acquire("k").is_ok());
    }
}
