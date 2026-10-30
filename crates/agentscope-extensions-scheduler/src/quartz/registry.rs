//! Process-wide registry mapping scheduler ids to running schedulers.
//!
//! Mirrors `QuartzAgentSchedulerRegistry` (a package-private `static` map in
//! Java). Rust has no JVM statics, so we use a `OnceLock`-guarded map.
//!
//! Java stores the `QuartzAgentScheduler` instance itself, which is what lets
//! `AgentQuartzJob` look its scheduler back up by the `schedulerId` carried in the
//! Quartz `JobDataMap`. The Rust equivalent keeps a [`Weak`] handle, so a lookup
//! returns the live scheduler (`Arc`) without the registry pinning a scheduler
//! that was dropped without `shutdown()`.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock, Weak};

use crate::quartz::QuartzAgentScheduler;

/// A handle recorded for each live scheduler.
#[derive(Clone)]
pub struct RegistryEntry {
    /// The scheduler id.
    pub id: String,
    /// Human-readable type, matching Java `getSchedulerType()` (e.g. `"quartz"`).
    pub scheduler_type: String,
    /// Weak reference used for the reverse lookup.
    handle: Weak<QuartzAgentScheduler>,
}

impl RegistryEntry {
    /// Upgrades the weak handle: `None` once the scheduler has been dropped.
    pub fn handle(&self) -> Option<Arc<QuartzAgentScheduler>> {
        self.handle.upgrade()
    }

    /// Whether the scheduler behind this entry is still alive.
    pub fn is_alive(&self) -> bool {
        self.handle.strong_count() > 0
    }
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

    /// Registers the scheduler under its own id, recording the Quartz type.
    pub fn register(&self, scheduler: &Arc<QuartzAgentScheduler>) {
        self.register_with_type(scheduler, "quartz");
    }

    /// Registers the scheduler under its own id with an explicit type name.
    pub fn register_with_type(
        &self,
        scheduler: &Arc<QuartzAgentScheduler>,
        scheduler_type: impl Into<String>,
    ) {
        let id = scheduler.scheduler_id().to_string();
        self.inner.write().unwrap().insert(
            id.clone(),
            RegistryEntry {
                id,
                scheduler_type: scheduler_type.into(),
                handle: Arc::downgrade(scheduler),
            },
        );
    }

    /// Removes the entry for `id`.
    pub fn unregister(&self, id: &str) {
        self.inner.write().unwrap().remove(id);
    }

    /// Whether a **live** scheduler with `id` is registered.
    ///
    /// An entry whose scheduler was dropped without `unregister` is pruned here.
    pub fn contains(&self, id: &str) -> bool {
        self.get_handle(id).is_some()
    }

    /// Fetches a clone of the entry for `id` (dead entries are pruned).
    pub fn get(&self, id: &str) -> Option<RegistryEntry> {
        let entry = self.inner.read().unwrap().get(id).cloned()?;
        if entry.is_alive() {
            Some(entry)
        } else {
            self.inner.write().unwrap().remove(id);
            None
        }
    }

    /// Reverse lookup: the live scheduler registered under `id`.
    ///
    /// This is the Java `QuartzAgentSchedulerRegistry.get(id)` equivalent that a
    /// job uses to reach the scheduler which created it.
    pub fn get_handle(&self, id: &str) -> Option<Arc<QuartzAgentScheduler>> {
        self.get(id)?.handle()
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
    use crate::test_support::EchoFactory;

    #[test]
    fn register_lookup_unregister() {
        let reg = SchedulerRegistry::new();
        let sched = Arc::new(QuartzAgentScheduler::new(Arc::new(EchoFactory)));
        let id = sched.scheduler_id().to_string();
        reg.register_with_type(&sched, "quartz");

        assert!(reg.contains(&id));
        let entry = reg.get(&id).unwrap();
        assert_eq!(entry.id, id);
        assert_eq!(entry.scheduler_type, "quartz");
        // The handle resolves back to the very same scheduler instance.
        let handle = entry.handle().expect("live handle");
        assert!(Arc::ptr_eq(&handle, &sched));
        assert!(Arc::ptr_eq(&reg.get_handle(&id).unwrap(), &sched));

        reg.unregister(&id);
        assert!(!reg.contains(&id));
        assert!(reg.get_handle(&id).is_none());
        assert!(reg.is_empty());
    }

    #[test]
    fn entry_dies_with_the_scheduler_and_is_pruned() {
        let reg = SchedulerRegistry::new();
        let id = {
            let sched = Arc::new(QuartzAgentScheduler::new(Arc::new(EchoFactory)));
            let id = sched.scheduler_id().to_string();
            reg.register(&sched);
            assert_eq!(reg.get_handle(&id).unwrap().scheduler_id(), id);
            id
        };

        // Dropping the only strong reference invalidates the entry: the weak
        // handle no longer upgrades and the lookup prunes it instead of handing
        // out a stale record.
        assert!(reg.get_handle(&id).is_none());
        assert!(!reg.contains(&id));
        assert!(reg.is_empty());
    }

    #[test]
    fn singleton_is_stable() {
        let a = scheduler_registry();
        let b = scheduler_registry();
        assert!(Arc::ptr_eq(&a, &b));
    }
}
