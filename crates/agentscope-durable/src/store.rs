//! The two storage primitives every backend is built on.
//!
//! A `RecordStore` gives one serialized transaction per session scope with a
//! store-authoritative clock; every durable invariant rests on that pairing.

pub mod file;
pub mod memory;

use std::collections::BTreeMap;
use std::ops::Bound;

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

    /// Bytes one scope document occupies right now.
    ///
    /// `None` when the store cannot tell (an in-memory map, a remote KV). It exists so the
    /// backend can decide *when* a scope is worth compacting without knowing what the store is;
    /// a store that answers `None` never triggers threshold compaction on its own.
    fn scope_bytes(&self, _scope: &str) -> Result<Option<u64>> {
        Ok(None)
    }

    /// The values of `parts` in one scope, for a caller that needs a few records and not a scope.
    ///
    /// A control poll happens several times a second while a model call streams, and it needs one
    /// record; reading the whole conversation for it is what made a hung provider cost tens of
    /// megabytes a second. A store that keeps a scope as one document overrides this with a scan
    /// that builds only the requested values (the file store does); the default reads the scope
    /// the ordinary way, which is correct for every store — including one whose records are
    /// already kept apart.
    fn read_parts(&self, scope: &str, parts: &[&str]) -> Result<Vec<Option<Value>>> {
        let mut values = vec![None; parts.len()];
        self.transaction(scope, &mut |txn: &mut dyn Txn| {
            for (slot, part) in values.iter_mut().zip(parts) {
                *slot = txn.get(part)?;
            }
            Ok(())
        })?;
        Ok(values)
    }

    fn shared_across_hosts(&self) -> bool;
}

/// The keys of one scope under `prefix` after `after`, with the transaction's staged writes
/// applied and `limit` honoured.
///
/// Both maps are ordered and the keys that share a prefix are contiguous in that order, so the
/// walk covers two ranges and stops as soon as the page is full. The previous shape collected
/// every key of the scope, filtered and sorted it: "the next page under this prefix" then cost
/// the whole history, which is what made reading a long journal quadratic.
pub(crate) fn merge_keys(
    base: &BTreeMap<String, Value>,
    staged: &BTreeMap<String, Option<Value>>,
    prefix: &str,
    after: Option<&str>,
    limit: usize,
) -> Vec<String> {
    // `range` starts strictly *above* its bound; a cursor that is not under this prefix at all
    // selects the prefix itself, so the walk never leaves the keys it is asked about.
    let from = match after {
        Some(after) if after >= prefix => Bound::Excluded(after),
        _ => Bound::Included(prefix),
    };
    let range = (from, Bound::Unbounded);
    // Only what this transaction wrote is materialized, and a write is a handful of records; the
    // document itself is walked lazily, one page and no more.
    let staged_keys: Vec<(&str, bool)> = staged
        .range::<str, _>(range)
        .take_while(|(key, _)| key.starts_with(prefix))
        .map(|(key, value)| (key.as_str(), value.is_some()))
        .collect();
    let mut written = 0usize;
    let mut keys = Vec::new();
    'walk: for (key, _) in base.range::<str, _>(range) {
        if !key.starts_with(prefix) {
            break;
        }
        // Keys this transaction wrote before `key` come first in order; a `put(None)` drops one.
        while let Some((staged_key, live)) = staged_keys.get(written) {
            if *staged_key >= key.as_str() {
                break;
            }
            if *live {
                keys.push((*staged_key).to_string());
            }
            written += 1;
            if keys.len() == limit {
                break 'walk;
            }
        }
        let staged_here = staged_keys
            .get(written)
            .filter(|(staged_key, _)| *staged_key == key.as_str());
        match staged_here {
            // Staged wins over the document it was written against.
            Some((staged_key, live)) => {
                if *live {
                    keys.push((*staged_key).to_string());
                }
                written += 1;
            }
            None => keys.push(key.clone()),
        }
        if keys.len() == limit {
            break;
        }
    }
    if keys.len() < limit {
        for (staged_key, live) in &staged_keys[written..] {
            if *live {
                keys.push((*staged_key).to_string());
            }
            if keys.len() == limit {
                break;
            }
        }
    }
    crate::traffic::count_keys(keys.len());
    keys
}

/// Runs `work` in a transaction and returns its value.
///
/// The closure is `FnOnce`; the store only needs `FnMut`, so the value is moved through an
/// `Option` and taken exactly once. A failed closure is handed back to the store as the
/// transaction outcome: only that makes the store discard the writes it staged.
pub fn txn<T>(
    store: &dyn RecordStore,
    scope: &str,
    work: impl FnOnce(&mut dyn Txn) -> Result<T>,
) -> Result<T> {
    let mut slot: Option<T> = None;
    let mut pending = Some(work);
    let mut runner = |transaction: &mut dyn Txn| -> Result<()> {
        let work = pending
            .take()
            .ok_or_else(|| crate::DurableError::unavailable("transaction ran twice"))?;
        slot = Some(work(transaction)?);
        Ok(())
    };
    store.transaction(scope, &mut runner)?;
    match slot {
        Some(value) => Ok(value),
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
