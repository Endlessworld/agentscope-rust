//! Node-level mutual-exclusion seam for multi-instance deployments.
//!
//! Java gets this from Quartz: with a JDBC job store and clustering enabled,
//! Quartz's own cluster lock lets exactly one node fire a given trigger. This
//! crate keeps its tasks in a process-local map ([`crate::quartz`]), so **out of
//! the box it assumes a single scheduler instance** (documented at the crate
//! root). A host that really runs several instances injects a
//! [`SchedulerLock`] backed by its storage layer (JDBC `NativeLockSession`,
//! Redis `SET NX`, …) and the Quartz job loop consults it before every automatic
//! firing, so the same slot runs on one node only.
//!
//! The default [`NoopSchedulerLock`] grants everything, which is exactly the
//! single-instance premise: no behaviour change, no extra dependency.

use async_trait::async_trait;

/// Cross-instance lock consulted before every automatic firing.
///
/// The `key` identifies one firing *slot* across nodes
/// (`<scheduler_id>:<task name>:<slot>`); two instances that computed the same
/// slot derive the same key, so `false` means "another node owns this firing".
/// Manual triggers ([`crate::quartz::QuartzAgentScheduler::trigger`]) and
/// `ScheduleAgentTask::run` bypass the lock, mirroring Java `triggerJob`, which
/// fires on the local instance only.
#[async_trait]
pub trait SchedulerLock: Send + Sync {
    /// Tries to take the lock for one firing slot.
    ///
    /// Returning `false` skips this firing on the current node (another node
    /// owns it, or the lock is unavailable); the job loop keeps its schedule.
    async fn try_acquire(&self, key: &str) -> bool;

    /// Releases the lock granted for `key` after the firing completed.
    async fn release(&self, key: &str);
}

/// Default lock: every firing is granted (single-instance deployment).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSchedulerLock;

impl NoopSchedulerLock {
    /// Creates the no-op lock.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SchedulerLock for NoopSchedulerLock {
    async fn try_acquire(&self, _key: &str) -> bool {
        true
    }

    async fn release(&self, _key: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn noop_lock_grants_every_slot() {
        let lock = NoopSchedulerLock::new();
        assert!(lock.try_acquire("sched:A:cron:1").await);
        assert!(lock.try_acquire("").await);
        // Releasing is a no-op and must not panic.
        lock.release("sched:A:cron:1").await;

        // Usable through the trait object the job context stores.
        let dynamic: Arc<dyn SchedulerLock> = Arc::new(NoopSchedulerLock);
        assert!(dynamic.try_acquire("sched:B:rate:2").await);
    }
}
