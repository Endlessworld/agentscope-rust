//! The two storage primitives every backend is built on.
//!
//! A `RecordStore` gives one serialized transaction per session scope with a
//! store-authoritative clock; every durable invariant rests on that pairing.

pub mod file;
pub mod memory;

use serde_json::Value;

use crate::Result;

/// A single session-scoped transaction. `now()` is the only time source.
pub trait Txn {
    fn now(&self) -> i64;
    fn get(&self, part: &str) -> Result<Option<Value>>;
    fn put(&mut self, part: &str, value: Option<Value>) -> Result<()>;
    fn keys(&self, prefix: &str, after: Option<&str>, limit: usize) -> Result<Vec<String>>;
    /// `ready_at < 0` removes the entry.
    fn schedule(
        &mut self,
        queue: &str,
        work_id: &str,
        ready_at: i64,
        payload: Option<Value>,
    ) -> Result<()>;
}

/// One entry of the durable readiness index.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkItem {
    pub queue: String,
    pub work_id: String,
    pub ready_at: i64,
    pub payload: Option<Value>,
}

/// A KV store with session-scoped transactions and a readiness index.
pub trait RecordStore: Send + Sync {
    /// Runs `work` atomically. Any error discards every change made by the closure.
    fn transaction(
        &self,
        scope: &str,
        work: &mut dyn FnMut(&mut dyn Txn) -> Result<()>,
    ) -> Result<()>;

    /// Entries that are due at or before `now`, ordered by readiness.
    fn ready(&self, queue: &str, now: i64, limit: usize) -> Result<Vec<WorkItem>>;

    fn shared_across_hosts(&self) -> bool;
}

/// Runs `work` in a transaction and returns its value.
///
/// The closure is `FnOnce`; the store only needs `FnMut`, so the value is moved through an
/// `Option` and taken exactly once.
pub fn txn<T>(
    store: &dyn RecordStore,
    scope: &str,
    work: impl FnOnce(&mut dyn Txn) -> Result<T>,
) -> Result<T> {
    let mut slot: Option<Result<T>> = None;
    let mut pending = Some(work);
    let mut runner = |transaction: &mut dyn Txn| -> Result<()> {
        let work = pending
            .take()
            .ok_or_else(|| crate::DurableError::unavailable("transaction ran twice"))?;
        slot = Some(work(transaction));
        Ok(())
    };
    store.transaction(scope, &mut runner)?;
    match slot {
        Some(value) => value,
        None => Err(crate::DurableError::unavailable("transaction did not run")),
    }
}

/// Read-only convenience over [`txn`] for call sites that need several parts at once.
pub fn read<T>(
    store: &dyn RecordStore,
    scope: &str,
    work: impl FnOnce(&mut dyn Txn) -> Result<T>,
) -> Result<T> {
    txn(store, scope, work)
}
