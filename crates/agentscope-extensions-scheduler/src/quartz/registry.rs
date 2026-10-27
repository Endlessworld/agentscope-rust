//! Process-wide registry mapping scheduler ids to running schedulers.
//!
//! Mirrors `QuartzAgentSchedulerRegistry` (a package-private `static` map in
//! Java). Rust has no JVM statics, so we use a `OnceLock`-guarded map.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

/// A lightweight handle recorded for each live scheduler.
#[derive(Clone)]
pub struct RegistryEntry {
    /// The scheduler id.
    pub id: String,
    /// Human-readable type (e.g. `"quartz"`).
    pub scheduler_type: String,
}

/// Registry of live scheduler instances keyed by id.
pub struct SchedulerRegistry {
    inner: RwLock<HashMap<String, RegistryEntry>>,
}

impl SchedulerRegistry {
    fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Registers (or replaces) the entry for `id`.
    pub fn register(&self, id: impl Into<String>, scheduler_type: impl Into<String>) {
        let id = id.into();
        self.inner.write().unwrap().insert(
            id.clone(),
            RegistryEntry {
                id,
                scheduler_type: scheduler_type.into(),
            },
        );
    }

    /// Removes the entry for `id`.
    pub fn unregister(&self, id: &str) {
        self.inner.write().unwrap().remove(id);
    }

    /// Returns true when a scheduler with `id` is registered.
    pub fn contains(&self, id: &str) -> bool {
        self.inner.read().unwrap().contains_key(id)
    }

    /// Fetches a clone of the entry for `id`.
    pub fn get(&self, id: &str) -> Option<RegistryEntry> {
        self.inner.read().unwrap().get(id).cloned()
    }

    /// Number of registered schedulers.
    pub fn len(&self) -> usize {
        self.inner.read().unwrap().len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().is_empty()
    }
}

static REGISTRY: OnceLock<Arc<SchedulerRegistry>> = OnceLock::new();

/// Returns the process-wide scheduler registry singleton.
pub fn scheduler_registry() -> Arc<SchedulerRegistry> {
    REGISTRY
        .get_or_init(|| Arc::new(SchedulerRegistry::new()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_lookup_unregister() {
        let reg = SchedulerRegistry::new();
        reg.register("s1", "quartz");
        assert!(reg.contains("s1"));
        assert_eq!(reg.get("s1").unwrap().scheduler_type, "quartz");
        reg.unregister("s1");
        assert!(!reg.contains("s1"));
    }

    #[test]
    fn singleton_is_stable() {
        let a = scheduler_registry();
        let b = scheduler_registry();
        assert!(Arc::ptr_eq(&a, &b));
    }
}
