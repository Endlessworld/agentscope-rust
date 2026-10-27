//! Distributed periodic coordination, mirroring
//! `io.agentscope.harness.agent.coordination`.
//!
//! A [`PeriodicGate`] is a minimum-gap throttle that coordinates periodic
//! background work across callers (and optionally across nodes). A successful
//! [`PeriodicGate::try_claim`] means the caller may proceed; `false` means another
//! caller recently claimed the slot or the gap has not elapsed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use crate::filesystem::remote::store::{BaseStore, StoreItem, StoreValue};

const EPOCH: SystemTime = SystemTime::UNIX_EPOCH;

/// Coordinates periodic background work using a minimum-gap throttle.
pub trait PeriodicGate: Send + Sync {
    /// Attempts to claim the named periodic slot.
    ///
    /// Returns `true` if the caller won the claim and may proceed, `false` otherwise.
    fn try_claim(&self, name: &str, min_gap: Duration) -> bool;
}

fn millis_now(now: SystemTime) -> i64 {
    now.duration_since(EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Shared process-wide claim map, mirroring the Java static map so the throttle
/// window survives across `HarnessAgent::builder().build()` calls.
fn shared_claims() -> &'static Mutex<HashMap<String, SystemTime>> {
    static SHARED: OnceLock<Mutex<HashMap<String, SystemTime>>> = OnceLock::new();
    SHARED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// JVM-local / process-local [`PeriodicGate`] backed by a shared map.
///
/// The map is process-wide (static) so the throttle window survives across agent
/// rebuilds — an instance-level map would reset on every request.
pub struct LocalPeriodicGate {
    /// `None` uses the shared static map; `Some` is an isolated map for tests.
    claims: Option<Arc<Mutex<HashMap<String, SystemTime>>>>,
}

impl Default for LocalPeriodicGate {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalPeriodicGate {
    pub fn new() -> Self {
        Self { claims: None }
    }

    /// Creates a gate backed by an isolated map (useful for tests).
    pub fn new_isolated() -> Self {
        Self {
            claims: Some(Arc::new(Mutex::new(HashMap::new()))),
        }
    }

    fn with_map<T>(&self, f: impl FnOnce(&mut HashMap<String, SystemTime>) -> T) -> T {
        match &self.claims {
            Some(map) => f(&mut map.lock().unwrap()),
            None => f(&mut shared_claims().lock().unwrap()),
        }
    }

    /// Removes entries whose last claim is older than `max_age`.
    pub fn cleanup_stale_entries(&self, max_age: Duration) {
        let cutoff = SystemTime::now().checked_sub(max_age).unwrap_or(EPOCH);
        self.with_map(|map| {
            map.retain(|_, last| *last >= cutoff);
        });
    }

    /// Clears all entries — for unit tests only.
    pub fn clear_for_tests(&self) {
        self.with_map(|map| map.clear());
    }

    /// Seeds a claim timestamp — for unit tests only.
    pub fn seed_last_claim_at_for_tests(&self, name: &str, instant: Option<SystemTime>) {
        self.with_map(|map| {
            map.insert(name.to_string(), instant.unwrap_or(EPOCH));
        });
    }

    /// Returns whether a named slot exists — for unit tests only.
    pub fn has_claim_for_tests(&self, name: &str) -> bool {
        self.with_map(|map| map.contains_key(name))
    }
}

impl PeriodicGate for LocalPeriodicGate {
    fn try_claim(&self, name: &str, min_gap: Duration) -> bool {
        if name.trim().is_empty() {
            return false;
        }
        let now = SystemTime::now();
        self.with_map(|map| {
            let last = map.get(name).copied().unwrap_or(EPOCH);
            if now.duration_since(last).unwrap_or_default() < min_gap {
                return false;
            }
            map.insert(name.to_string(), now);
            true
        })
    }
}

/// Distributed [`PeriodicGate`] backed by [`BaseStore::put_if_version`], enabling
/// cross-node coordination of periodic background work.
pub struct StoreBackedPeriodicGate {
    store: Arc<dyn BaseStore>,
}

const NAMESPACE: [&str; 2] = ["coordination", "periodic"];
const MAX_CAS_RETRIES: usize = 5;

impl StoreBackedPeriodicGate {
    pub fn new(store: Arc<dyn BaseStore>) -> Self {
        Self { store }
    }

    fn namespace() -> Vec<String> {
        NAMESPACE.iter().map(|s| s.to_string()).collect()
    }

    fn read_last_claim_at(item: Option<&StoreItem>) -> SystemTime {
        let Some(ts) = item.and_then(|i| i.value.get("lastClaimAt")) else {
            return EPOCH;
        };
        let Some(millis) = ts.as_i64() else {
            return EPOCH;
        };
        if millis <= 0 {
            return EPOCH;
        }
        EPOCH + Duration::from_millis(millis as u64)
    }
}

impl PeriodicGate for StoreBackedPeriodicGate {
    fn try_claim(&self, name: &str, min_gap: Duration) -> bool {
        if name.trim().is_empty() {
            return false;
        }
        let now = SystemTime::now();
        let ns = Self::namespace();
        for _ in 0..MAX_CAS_RETRIES {
            let item = self.store.get(&ns, name);
            let expected_version = item.as_ref().map_or(0, |i| i.version);
            let last = Self::read_last_claim_at(item.as_ref());
            if now.duration_since(last).unwrap_or_default() < min_gap {
                return false;
            }
            let mut value = StoreValue::new();
            value.insert(
                "lastClaimAt".to_string(),
                serde_json::Value::Number(serde_json::Number::from(millis_now(now))),
            );
            if self
                .store
                .put_if_version(&ns, name, value, expected_version)
            {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::remote::store::InMemoryStore;

    #[test]
    fn local_gate_throttles_within_min_gap() {
        let gate = LocalPeriodicGate::new_isolated();
        assert!(gate.try_claim("memory-flush", Duration::from_secs(60)));
        assert!(!gate.try_claim("memory-flush", Duration::from_secs(60)));
        // Different slot is independent.
        assert!(gate.try_claim("skill-curator", Duration::from_secs(60)));
    }

    #[test]
    fn local_gate_allows_after_gap_elapsed() {
        let gate = LocalPeriodicGate::new_isolated();
        gate.seed_last_claim_at_for_tests("t", Some(SystemTime::now() - Duration::from_secs(120)));
        assert!(gate.try_claim("t", Duration::from_secs(60)));
    }

    #[test]
    fn local_gate_rejects_blank_name() {
        let gate = LocalPeriodicGate::new_isolated();
        assert!(!gate.try_claim("  ", Duration::from_secs(1)));
    }

    #[test]
    fn local_gate_cleanup_removes_stale_entries() {
        let gate = LocalPeriodicGate::new_isolated();
        gate.seed_last_claim_at_for_tests(
            "old",
            Some(SystemTime::now() - Duration::from_secs(600)),
        );
        gate.seed_last_claim_at_for_tests("new", Some(SystemTime::now()));
        assert!(gate.has_claim_for_tests("old"));
        gate.cleanup_stale_entries(Duration::from_secs(300));
        assert!(!gate.has_claim_for_tests("old"));
        assert!(gate.has_claim_for_tests("new"));
    }

    #[test]
    fn store_backed_gate_uses_cas() {
        let store = Arc::new(InMemoryStore::new());
        let gate = StoreBackedPeriodicGate::new(store.clone());
        assert!(gate.try_claim("curator", Duration::from_secs(60)));
        assert!(!gate.try_claim("curator", Duration::from_secs(60)));
        // A different slot on the same store is independent.
        assert!(gate.try_claim("memory", Duration::from_secs(60)));
        // Version was bumped on the store by the successful claim.
        let ns = StoreBackedPeriodicGate::namespace();
        assert!(store.get(&ns, "curator").is_some());
    }

    #[test]
    fn store_backed_gate_rejects_blank_name() {
        let store = Arc::new(InMemoryStore::new());
        let gate = StoreBackedPeriodicGate::new(store);
        assert!(!gate.try_claim("", Duration::from_secs(1)));
    }
}
